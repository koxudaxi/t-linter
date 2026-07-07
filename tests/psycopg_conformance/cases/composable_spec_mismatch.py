# expect: sql-composable-spec-mismatch
from typing import Annotated
from string.templatelib import Template
from psycopg import sql

TEMPLATE: Annotated[Template, "sql"] = t"SELECT * FROM {sql.Identifier('users')}"
