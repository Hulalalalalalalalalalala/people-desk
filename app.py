#!/usr/bin/env python3
"""PeopleDesk HTTP service."""
import argparse
import json
import re
import signal
import sqlite3
from datetime import date
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import urlsplit

PRODUCT = "PeopleDesk"
RESOURCE = "employees"
ACTIVE_STATUS = "在职"
DATE_RE = re.compile(r"^(\d{4})-(\d{2})-(\d{2})$")

# (规范 JSON 字段, 兼容字段, 中文标签)
REQUIRED_FIELDS = (
    ("employeeNo", "employee_no", "工号"),
    ("name", "name", "姓名"),
    ("department", "department", "部门"),
    ("position", "position", "岗位"),
)
OPTIONAL_FIELDS = (
    ("phone", "phone", "电话"),
    ("email", "email", "邮箱"),
)
COLUMNS = (
    "id",
    "employee_no",
    "name",
    "department",
    "position",
    "status",
    "effective_date",
    "phone",
    "email",
)

PAGE = """<!doctype html>
<html lang="zh-CN">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>PeopleDesk · 员工档案与招聘出勤</title>
<style>
body{font-family:system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem;line-height:1.7;color:#1f2933}
a{color:#175b9c}
form{display:grid;grid-template-columns:repeat(auto-fit,minmax(14rem,1fr));gap:.75rem 1rem;margin:1rem 0}
label{display:flex;flex-direction:column;font-size:.9rem;gap:.25rem}
input{padding:.45rem .55rem;font:inherit;border:1px solid #b7c0cc;border-radius:.35rem}
input.invalid{border-color:#c0392b}
.hint{color:#6b7684;font-weight:normal}
.actions{grid-column:1/-1;display:flex;align-items:center;gap:1rem;flex-wrap:wrap}
button{padding:.5rem 1.2rem;font:inherit;border:0;border-radius:.35rem;background:#175b9c;color:#fff;cursor:pointer}
button:disabled{opacity:.6;cursor:default}
.message{margin:0;min-height:1.2em}
.message.error{color:#c0392b}
.message.success{color:#1e7d43}
.field-error{color:#c0392b;font-size:.8rem;min-height:1em}
table{border-collapse:collapse;width:100%;margin-top:.5rem;font-size:.92rem}
th,td{border:1px solid #d7dde5;padding:.45rem .6rem;text-align:left;vertical-align:top}
th{background:#f2f5f8}
</style>
<main>
<h1>PeopleDesk</h1>
<p>员工档案与招聘出勤</p>

<h2>新建员工档案</h2>
<form id="employee-form" novalidate>
  <label>工号
    <input name="employeeNo" autocomplete="off" maxlength="50">
    <span class="field-error" data-error="employeeNo"></span>
  </label>
  <label>姓名
    <input name="name" autocomplete="off" maxlength="50">
    <span class="field-error" data-error="name"></span>
  </label>
  <label>部门
    <input name="department" autocomplete="off" maxlength="50">
    <span class="field-error" data-error="department"></span>
  </label>
  <label>岗位
    <input name="position" autocomplete="off" maxlength="50">
    <span class="field-error" data-error="position"></span>
  </label>
  <label>任职生效日期 <span class="hint">YYYY-MM-DD</span>
    <input name="effectiveDate" placeholder="2026-10-03" autocomplete="off" maxlength="10">
    <span class="field-error" data-error="effectiveDate"></span>
  </label>
  <label>电话 <span class="hint">可选</span>
    <input name="phone" autocomplete="off" maxlength="50">
    <span class="field-error" data-error="phone"></span>
  </label>
  <label>邮箱 <span class="hint">可选</span>
    <input name="email" autocomplete="off" maxlength="100">
    <span class="field-error" data-error="email"></span>
  </label>
  <div class="actions">
    <button type="submit" id="submit-btn">提交</button>
    <p class="message" id="form-message" role="alert"></p>
  </div>
</form>

<h2>员工列表</h2>
<p id="empty-tip">还没有员工记录。请在上方表单录入第一名员工。</p>
<table id="employee-table" hidden>
  <thead>
    <tr><th>工号</th><th>姓名</th><th>部门</th><th>岗位</th><th>状态</th><th>生效日期</th><th>电话</th><th>邮箱</th></tr>
  </thead>
  <tbody id="employee-tbody"></tbody>
</table>

<p><a href="/api/employees">查看员工列表接口</a> · <a href="/health">服务状态</a></p>
</main>
<script>
(function () {
  var FIELDS = ["employeeNo", "name", "department", "position", "effectiveDate", "phone", "email"];
  var form = document.getElementById("employee-form");
  var button = document.getElementById("submit-btn");
  var message = document.getElementById("form-message");

  function setMessage(text, kind) {
    message.textContent = text || "";
    message.className = "message" + (kind ? " " + kind : "");
  }

  function clearFieldErrors() {
    var slots = form.querySelectorAll("[data-error]");
    for (var i = 0; i < slots.length; i++) {
      slots[i].textContent = "";
    }
    var inputs = form.querySelectorAll("input.invalid");
    for (var j = 0; j < inputs.length; j++) {
      inputs[j].classList.remove("invalid");
    }
  }

  function showFieldError(field, text) {
    var slot = form.querySelector('[data-error="' + field + '"]');
    if (slot) {
      slot.textContent = text;
      var input = form.elements[field];
      if (input) {
        input.classList.add("invalid");
      }
    }
  }

  function renderEmployees(employees) {
    var tip = document.getElementById("empty-tip");
    var table = document.getElementById("employee-table");
    var tbody = document.getElementById("employee-tbody");
    tbody.replaceChildren();
    if (!employees.length) {
      tip.hidden = false;
      table.hidden = true;
      return;
    }
    tip.hidden = true;
    table.hidden = false;
    employees.forEach(function (employee) {
      var tr = document.createElement("tr");
      function addCell(value) {
        var td = document.createElement("td");
        if (value === null || value === undefined || value === "") {
          td.textContent = "未填写";
        } else {
          td.textContent = String(value);
        }
        tr.appendChild(td);
      }
      addCell(employee.employeeNo);
      addCell(employee.name);
      addCell(employee.department);
      addCell(employee.position);
      addCell(employee.status);
      addCell(employee.effectiveDate);
      addCell(employee.phone);
      addCell(employee.email);
      tbody.appendChild(tr);
    });
  }

  function loadEmployees() {
    fetch("/api/employees", { headers: { "Accept": "application/json" } })
      .then(function (resp) { return resp.json(); })
      .then(function (data) {
        renderEmployees(Array.isArray(data.employees) ? data.employees : []);
      })
      .catch(function () {
        renderEmployees([]);
      });
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    clearFieldErrors();
    setMessage("");
    var payload = {};
    FIELDS.forEach(function (field) {
      payload[field] = form.elements[field].value;
    });
    button.disabled = true;
    fetch("/api/employees", {
      method: "POST",
      headers: { "Content-Type": "application/json", "Accept": "application/json" },
      body: JSON.stringify(payload)
    })
      .then(function (resp) {
        return resp.json().catch(function () { return {}; }).then(function (data) {
          return { status: resp.status, data: data };
        });
      })
      .then(function (result) {
        if (result.status === 201) {
          form.reset();
          var savedName = result.data && result.data.name ? result.data.name : "新员工";
          setMessage("已保存员工档案：" + savedName, "success");
          loadEmployees();
        } else {
          var error = result.data && result.data.error ? result.data.error : "提交失败，请检查填写内容。";
          setMessage(error, "error");
          if (result.data && result.data.field) {
            showFieldError(result.data.field, error);
          }
        }
      })
      .catch(function () {
        setMessage("网络错误，请稍后重试。", "error");
      })
      .then(function () {
        button.disabled = false;
      });
  });

  loadEmployees();
})();
</script>
</html>
"""


class EmployeeError(ValueError):
    """A field-level validation error for a submitted employee."""

    def __init__(self, field, message):
        super().__init__(message)
        self.field = field
        self.message = message


def pick_field(obj, *keys):
    for key in keys:
        if key in obj:
            return obj[key], True
    return None, False


def is_real_calendar_date(value):
    match = DATE_RE.match(value)
    if match is None:
        return False
    try:
        date(int(match.group(1)), int(match.group(2)), int(match.group(3)))
    except ValueError:
        return False
    return True


def validate_employee(obj):
    """Validate a posted JSON object and return cleaned values.

    Raises EmployeeError on the first field problem. Nothing is written
    before validation succeeds, so a rejected request leaves no partial data.
    """
    cleaned = {}

    for canonical, alias, label in REQUIRED_FIELDS:
        value, present = pick_field(obj, canonical, alias)
        if not present:
            raise EmployeeError(canonical, f"{label}不能为空")
        if not isinstance(value, str):
            raise EmployeeError(canonical, f"{label}必须是字符串")
        text = value.strip()
        if not text:
            raise EmployeeError(canonical, f"{label}不能为空")
        cleaned[canonical] = text

    value, present = pick_field(obj, "effectiveDate", "effective_date")
    if not present:
        raise EmployeeError("effectiveDate", "任职生效日期不能为空")
    if not isinstance(value, str):
        raise EmployeeError("effectiveDate", "任职生效日期必须是字符串")
    effective_date = value.strip()
    if not effective_date:
        raise EmployeeError("effectiveDate", "任职生效日期不能为空")
    if not is_real_calendar_date(effective_date):
        raise EmployeeError(
            "effectiveDate",
            "任职生效日期必须是 YYYY-MM-DD 形式的真实日历日期",
        )
    cleaned["effectiveDate"] = effective_date

    for canonical, _alias, label in OPTIONAL_FIELDS:
        value, present = pick_field(obj, canonical)
        if not present:
            cleaned[canonical] = None
            continue
        if not isinstance(value, str):
            raise EmployeeError(canonical, f"{label}必须是字符串")
        text = value.strip()
        cleaned[canonical] = text or None

    return cleaned


def serialize_employee(row):
    return {
        "id": row["id"],
        "employeeNo": row["employee_no"],
        "name": row["name"],
        "department": row["department"],
        "position": row["position"],
        "status": row["status"],
        "effectiveDate": row["effective_date"],
        "phone": row["phone"],
        "email": row["email"],
    }


def main():
    parser = argparse.ArgumentParser(description="PeopleDesk - 员工档案与招聘出勤")
    commands = parser.add_subparsers(dest="command", required=True)
    serve = commands.add_parser("serve", help="Start the HTTP service")
    serve.add_argument("--host", default="127.0.0.1", help="Address to bind (default: 127.0.0.1)")
    serve.add_argument("--port", type=int, default=8080, help="Port to bind; 0 selects an available port")
    serve.add_argument("--data-dir", type=Path, default=Path("data"), help="Directory for the local SQLite database")
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("port must be between 0 and 65535")
    args.data_dir.mkdir(parents=True, exist_ok=True)
    database = sqlite3.connect(args.data_dir / "people-desk.sqlite")
    database.row_factory = sqlite3.Row
    database.execute("CREATE TABLE IF NOT EXISTS employees (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
    # Migrate databases created before employee profiles existed; old rows keep
    # their original ids and simply have NULL for the new columns.
    existing_columns = {row[1] for row in database.execute("PRAGMA table_info(employees)")}
    for column in ("employee_no", "department", "position", "status", "effective_date", "phone", "email"):
        if column not in existing_columns:
            database.execute(f"ALTER TABLE employees ADD COLUMN {column} TEXT")
    database.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_employees_no_key "
        "ON employees (LOWER(employee_no))"
    )
    database.commit()

    select_sql = (
        "SELECT id, employee_no, name, department, position, status, "
        "effective_date, phone, email FROM employees ORDER BY id ASC"
    )

    class Handler(BaseHTTPRequestHandler):
        def respond(self, status, value, *, html=False, allow=None):
            payload = value.encode("utf8") if html else json.dumps(value, ensure_ascii=False).encode("utf8")
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8" if html else "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            if status == 405:
                self.send_header("Allow", allow or "GET")
            self.end_headers()
            self.wfile.write(payload)

        def read_json_object(self):
            try:
                length = int(self.headers.get("Content-Length", "0") or "0")
            except ValueError:
                length = 0
            raw = self.rfile.read(length) if length > 0 else b""
            try:
                value = json.loads(raw.decode("utf8"))
            except (ValueError, UnicodeDecodeError):
                return None, "请求体不是有效的 JSON 对象"
            if not isinstance(value, dict):
                return None, "请求体必须是一个 JSON 对象"
            return value, None

        def list_employees(self):
            records = [serialize_employee(row) for row in database.execute(select_sql)]
            self.respond(200, {RESOURCE: records})

        def create_employee(self):
            obj, problem = self.read_json_object()
            if problem is not None:
                self.respond(400, {"error": problem})
                return
            try:
                cleaned = validate_employee(obj)
            except EmployeeError as exc:
                self.respond(400, {"error": exc.message, "field": exc.field})
                return

            employee_no = cleaned["employeeNo"]
            duplicate_error = {"error": f"工号 {employee_no} 已存在", "field": "employeeNo"}
            try:
                with database:
                    clash = database.execute(
                        "SELECT id FROM employees WHERE LOWER(employee_no) = ?",
                        (employee_no.lower(),),
                    ).fetchone()
                    if clash is not None:
                        self.respond(409, duplicate_error)
                        return
                    cursor = database.execute(
                        "INSERT INTO employees "
                        "(employee_no, name, department, position, status, effective_date, phone, email) "
                        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        (
                            employee_no,
                            cleaned["name"],
                            cleaned["department"],
                            cleaned["position"],
                            ACTIVE_STATUS,
                            cleaned["effectiveDate"],
                            cleaned["phone"],
                            cleaned["email"],
                        ),
                    )
                    new_id = cursor.lastrowid
            except sqlite3.IntegrityError:
                self.respond(409, duplicate_error)
                return

            row = database.execute(select_sql.replace("ORDER BY id ASC", "WHERE id = ?"), (new_id,)).fetchone()
            self.respond(201, serialize_employee(row))

        def route(self):
            location = urlsplit(self.path).path
            if location not in ("/", "/health", "/api/employees"):
                self.respond(404, {"error": "not found"})
                return
            allowed = "GET, POST" if location == "/api/employees" else "GET"
            if location == "/api/employees" and self.command == "POST":
                self.create_employee()
                return
            if self.command != "GET":
                self.respond(405, {"error": "method not allowed"}, allow=allowed)
                return
            if location == "/":
                self.respond(200, PAGE, html=True)
            elif location == "/health":
                self.respond(200, {"status": "ok", "product": PRODUCT})
            else:
                self.list_employees()

        do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = route

    def stop(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    server = None
    try:
        server = HTTPServer((args.host, args.port), Handler)
        host, port = server.server_address[:2]
        address = f"[{host}]" if ":" in host else host
        print(f"{PRODUCT} listening on http://{address}:{port}", flush=True)
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        if server is not None:
            server.server_close()
        database.close()


if __name__ == "__main__":
    main()
