# expect: sql-dict-needs-json-wrapper
from typing import Annotated
from string.templatelib import Template

TEMPLATE: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE data = { {'a': 1} }"
