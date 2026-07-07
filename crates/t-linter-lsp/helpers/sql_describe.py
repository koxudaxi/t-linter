import json
import os
import sys


def _error_payload(message, position=None):
    payload = {"message": str(message)}
    if position is not None:
        try:
            payload["position"] = int(position)
        except (TypeError, ValueError):
            pass
    return payload


def _type_rows(conn, oids):
    if not oids:
        return {}
    rows = conn.execute(
        "SELECT oid, typname FROM pg_catalog.pg_type WHERE oid = ANY(%s::oid[])",
        (list(oids),),
    ).fetchall()
    return {int(oid): typname for oid, typname in rows}


def _consume_results(pgconn):
    results = []
    while True:
        result = pgconn.get_result()
        if result is None:
            break
        results.append(result)
    return results


def _describe_with_pq(conn, sql_text):
    pgconn = conn.pgconn
    name = f"__tl_describe_pq_{os.getpid()}".encode()
    sql_bytes = sql_text.encode(conn.info.encoding)

    pgconn.send_prepare(name, sql_bytes)
    prepare_results = _consume_results(pgconn)
    if prepare_results:
        status_name = getattr(prepare_results[-1].status, "name", str(prepare_results[-1].status))
        if status_name not in {"COMMAND_OK", "TUPLES_OK"}:
            message = prepare_results[-1].error_message.decode(conn.info.encoding, "replace")
            raise RuntimeError(message)

    result = pgconn.describe_prepared(name)
    param_oids = [int(result.param_type(i)) for i in range(result.nparams)]
    column_oids = [int(result.ftype(i)) for i in range(result.nfields)]
    names = _type_rows(conn, set(param_oids + column_oids))

    params = [
        {"oid": oid, "type_name": names.get(oid, f"oid_{oid}")}
        for oid in param_oids
    ]
    columns = [
        {
            "name": result.fname(i).decode(conn.info.encoding, "replace"),
            "oid": oid,
            "type_name": names.get(oid, f"oid_{oid}"),
        }
        for i, oid in enumerate(column_oids)
    ]
    conn.execute(f"DEALLOCATE {name.decode()}")
    return params, columns


def _describe_with_prepare(conn, sql_text):
    name = f"__tl_describe_prepare_{os.getpid()}"
    conn.execute(f"PREPARE {name} AS {sql_text}")
    rows = conn.execute(
        """
        SELECT t.oid, t.typname
        FROM pg_catalog.pg_prepared_statements p,
             unnest(p.parameter_types) WITH ORDINALITY AS u(type_oid, ord)
             JOIN pg_catalog.pg_type t ON t.oid = u.type_oid::oid
        WHERE p.name = %s
        ORDER BY u.ord
        """,
        (name,),
    ).fetchall()
    conn.execute(f"DEALLOCATE {name}")
    params = [{"oid": int(oid), "type_name": typname} for oid, typname in rows]
    return params, []


def _describe(database_url, sql_text, search_path):
    import psycopg

    with psycopg.connect(database_url, autocommit=True) as conn:
        if search_path:
            conn.execute("SELECT set_config('search_path', %s, false)", (search_path,))
        try:
            params, columns = _describe_with_pq(conn, sql_text)
        except Exception:
            params, columns = _describe_with_prepare(conn, sql_text)
        return {
            "params": params,
            "columns": columns,
            "psycopg_version": psycopg.__version__,
        }


def _handle(request):
    if (op := request.get("op")) != "describe":
        return {"error": _error_payload("unsupported op")}
    if not (database_url := request.get("database_url")):
        return {"error": _error_payload("database_url is required")}
    if not isinstance(sql_text := request.get("sql"), str):
        return {"error": _error_payload("sql is required")}
    try:
        return _describe(database_url, sql_text, request.get("search_path"))
    except Exception as exc:
        position = getattr(getattr(exc, "diag", None), "statement_position", None)
        return {"error": _error_payload(exc, position)}


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        response = {"id": request.get("id")}
        response.update(_handle(request))
        sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
