# expect: sql-format-spec-unknown
from typing import Annotated
from string.templatelib import Template

user_id = 1
TEMPLATE: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id:z}"
