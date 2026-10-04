#!/usr/bin/env python3
"""PeopleDesk HTTP service."""
import argparse
import json
from datetime import datetime
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import re
import signal
import sqlite3
from urllib.parse import urlsplit

PRODUCT = "PeopleDesk"
RESOURCE = "employees"
STATUS_ACTIVE = "在职"

PAGE = """<!doctype html>
<html lang="zh-CN">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>PeopleDesk · 员工档案与招聘出勤</title>
<style>
body{font-family:system-ui,sans-serif;max-width:56rem;margin:3rem auto;padding:0 1rem;line-height:1.7;color:#222}
h1{margin-bottom:.25rem}
h2{margin-top:2rem}
form{border:1px solid #ddd;border-radius:.5rem;padding:1rem 1.25rem;margin:1rem 0;background:#fafafa}
label{display:block;margin:.6rem 0 .2rem;font-weight:600}
input{padding:.4rem .5rem;border:1px solid #bbb;border-radius:.3rem;width:18rem;max-width:100%;font-size:.95rem}
button{margin-top:.9rem;padding:.5rem 1.2rem;border:0;border-radius:.3rem;background:#175b9c;color:#fff;font-size:.95rem;cursor:pointer}
table{border-collapse:collapse;width:100%;margin-top:1rem;font-size:.95rem}
th,td{border:1px solid #ddd;padding:.4rem .6rem;text-align:left;vertical-align:top}
th{background:#f4f4f4}
.msg{margin:.75rem 0 0;padding:.5rem .75rem;border-radius:.3rem}
.error{background:#fdecea;color:#9b1c1c;border:1px solid #f5c2c0}
.empty{color:#666}
a{color:#175b9c}
</style>
<main>
<h1>PeopleDesk</h1>
<p>员工档案与招聘出勤</p>
<h2>新建员工档案</h2>
<form id="emp-form">
  <label>工号 <input name="employee_no" required></label>
  <label>姓名 <input name="name" required></label>
  <label>部门 <input name="department" required></label>
  <label>岗位 <input name="position" required></label>
  <label>任职生效日期 <input name="effective_date" type="date" required></label>
  <label>电话（选填）<input name="phone"></label>
  <label>邮箱（选填）<input name="email"></label>
  <button type="submit">保存档案</button>
  <div id="form-error" class="msg error" hidden></div>
  <div id="form-note" class="msg error" hidden></div>
</form>
<h2>员工列表</h2>
<div id="list"><p class="empty">加载中……</p></div>
<p><a href="/api/employees">查看员工列表接口</a> · <a href="/health">服务状态</a></p>
</main>
<script>
function esc(s){
  return String(s==null?"":s).replace(/[&<>"']/g,function(c){
    var m={"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"};
    return m[c];
  });
}
function text(v){
  return (v===null||v===undefined||v==="")?"未填写":v;
}
function contact(e){
  var parts=[];
  if(e.phone){parts.push(e.phone);}
  if(e.email){parts.push(e.email);}
  return parts.length?parts.join(" · "):"未填写";
}
function render(employees){
  var root=document.getElementById("list");
  if(!employees.length){
    root.innerHTML='<p class="empty">还没有员工记录。</p>';
    return;
  }
  var html="<table><thead><tr><th>工号</th><th>姓名</th><th>部门</th><th>岗位</th><th>状态</th><th>生效日期</th><th>联系方式</th></tr></thead><tbody>";
  for(var i=0;i<employees.length;i++){
    var e=employees[i];
    html+="<tr><td>"+esc(text(e.employee_no))+"</td>"
      +"<td>"+esc(e.name)+"</td>"
      +"<td>"+esc(text(e.department))+"</td>"
      +"<td>"+esc(text(e.position))+"</td>"
      +"<td>"+esc(text(e.status))+"</td>"
      +"<td>"+esc(text(e.effective_date))+"</td>"
      +"<td>"+esc(contact(e))+"</td></tr>";
  }
  html+="</tbody></table>";
  root.innerHTML=html;
}
// 最后一次成功读取到的员工列表；null 表示还没有任何一次读取成功。
var lastEmployees=null;
function showLoadError(){
  var root=document.getElementById("list");
  if(lastEmployees&&lastEmployees.length){
    // 已有成功展示过的员工：保留上次记录，并明确说明本次未能取得最新列表。
    render(lastEmployees);
    var note=document.createElement("p");
    note.className="msg error";
    note.textContent="员工列表加载失败，请稍后重试。当前显示的是上次读取的记录，本次未能取得最新列表。";
    root.appendChild(note);
  }else{
    // 从未成功读取，或上次成功取得的就是空列表：只显示失败状态。
    root.innerHTML='<p class="msg error">员工列表加载失败，请稍后重试。</p>';
  }
}
function load(){
  return fetch("/api/employees")
    .then(function(r){
      if(!r.ok){throw new Error("list status "+r.status);}
      return r.json();
    })
    .then(function(data){
      if(!data||!Array.isArray(data.employees)){throw new Error("list payload invalid");}
      lastEmployees=data.employees;
      render(lastEmployees);
      return true;
    })
    .catch(function(){
      showLoadError();
      return false;
    });
}
document.getElementById("emp-form").addEventListener("submit",function(ev){
  ev.preventDefault();
  var form=ev.target;
  var errBox=document.getElementById("form-error");
  var noteBox=document.getElementById("form-note");
  errBox.hidden=true;
  noteBox.hidden=true;
  var body={
    employee_no:form.employee_no.value,
    name:form.name.value,
    department:form.department.value,
    position:form.position.value,
    effective_date:form.effective_date.value,
    phone:form.phone.value,
    email:form.email.value
  };
  fetch("/api/employees",{
    method:"POST",
    headers:{"Content-Type":"application/json"},
    body:JSON.stringify(body)
  }).then(function(r){
    return r.json().then(function(data){return {status:r.status,data:data};});
  }).then(function(res){
    if(res.status===201){
      form.reset();
      load().then(function(ok){
        if(!ok){
          // 档案已保存成功，仅列表刷新失败：不能当作保存失败。
          noteBox.textContent="档案已保存，但员工列表刷新失败，请稍后重试。";
          noteBox.hidden=false;
        }
      });
    }else{
      errBox.textContent=(res.data&&res.data.error)||"保存失败，请检查填写内容。";
      errBox.hidden=false;
    }
  }).catch(function(){
    errBox.textContent="保存失败，请稍后重试。";
    errBox.hidden=false;
  });
});
load();
</script>
</html>
"""

DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
REQUIRED_FIELDS = ("employee_no", "name", "department", "position", "effective_date")
FIELD_LABELS = {
    "employee_no": "工号",
    "name": "姓名",
    "department": "部门",
    "position": "岗位",
    "effective_date": "生效日期",
    "phone": "电话",
    "email": "邮箱",
}
EMPLOYEE_COLUMNS = (
    "id", "name", "employee_no", "department", "position",
    "effective_date", "phone", "email", "status",
)


def parse_effective_date(value):
    if not isinstance(value, str) or not DATE_RE.match(value):
        return None
    try:
        parsed = datetime.strptime(value, "%Y-%m-%d")
    except ValueError:
        return None
    if parsed.strftime("%Y-%m-%d") != value:
        return None
    return value


def employee_from_row(row):
    return dict(zip(EMPLOYEE_COLUMNS, row))


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
    database.execute("CREATE TABLE IF NOT EXISTS employees (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
    for column in ("employee_no", "department", "position", "effective_date", "phone", "email", "status"):
        try:
            database.execute(f"ALTER TABLE employees ADD COLUMN {column} TEXT")
        except sqlite3.OperationalError:
            pass
    database.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_employees_employee_no "
        "ON employees(lower(employee_no))"
    )
    database.commit()

    class Handler(BaseHTTPRequestHandler):
        def respond(self, status, value, *, html=False, allow=None):
            payload = value.encode("utf8") if html else json.dumps(value, ensure_ascii=False).encode("utf8")
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8" if html else "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            if allow is not None:
                self.send_header("Allow", allow)
            self.end_headers()
            self.wfile.write(payload)

        def list_employees(self):
            records = [
                employee_from_row(row)
                for row in database.execute(
                    "SELECT id, name, employee_no, department, position, "
                    "effective_date, phone, email, status "
                    "FROM employees ORDER BY id"
                )
            ]
            self.respond(200, {RESOURCE: records})

        def create_employee(self):
            try:
                length = int(self.headers.get("Content-Length") or 0)
            except (TypeError, ValueError):
                length = 0
            raw = self.rfile.read(length) if length > 0 else b""
            try:
                data = json.loads(raw.decode("utf-8"))
            except (ValueError, UnicodeDecodeError):
                self.respond(400, {"error": "请求体不是有效的 JSON 对象"})
                return
            if not isinstance(data, dict):
                self.respond(400, {"error": "请求体必须是 JSON 对象"})
                return

            for field in REQUIRED_FIELDS:
                label = FIELD_LABELS[field]
                if field not in data:
                    self.respond(400, {"error": f"{label}不能为空", "field": field})
                    return
                if not isinstance(data[field], str):
                    self.respond(400, {"error": f"{label}必须是字符串", "field": field})
                    return

            employee_no = data["employee_no"].strip()
            name = data["name"].strip()
            department = data["department"].strip()
            position = data["position"].strip()
            if not employee_no:
                self.respond(400, {"error": "工号不能为空", "field": "employee_no"})
                return
            if not name:
                self.respond(400, {"error": "姓名不能为空", "field": "name"})
                return
            if not department:
                self.respond(400, {"error": "部门不能为空", "field": "department"})
                return
            if not position:
                self.respond(400, {"error": "岗位不能为空", "field": "position"})
                return

            effective_date = parse_effective_date(data["effective_date"])
            if effective_date is None:
                self.respond(400, {
                    "error": "生效日期无效，必须是 YYYY-MM-DD 形式的真实日历日期",
                    "field": "effective_date",
                })
                return

            phone = data.get("phone", "")
            email = data.get("email", "")
            if not isinstance(phone, str):
                self.respond(400, {"error": "电话必须是字符串", "field": "phone"})
                return
            if not isinstance(email, str):
                self.respond(400, {"error": "邮箱必须是字符串", "field": "email"})
                return
            phone = phone.strip()
            email = email.strip()

            duplicate = database.execute(
                "SELECT 1 FROM employees WHERE employee_no IS NOT NULL AND lower(employee_no) = lower(?)",
                (employee_no,),
            ).fetchone()
            if duplicate is not None:
                self.respond(409, {"error": "该工号已存在", "field": "employee_no"})
                return

            try:
                with database:
                    cursor = database.execute(
                        "INSERT INTO employees "
                        "(name, employee_no, department, position, effective_date, phone, email, status) "
                        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        (name, employee_no, department, position,
                         effective_date, phone, email, STATUS_ACTIVE),
                    )
            except sqlite3.IntegrityError:
                self.respond(409, {"error": "该工号已存在", "field": "employee_no"})
                return

            row = database.execute(
                "SELECT id, name, employee_no, department, position, "
                "effective_date, phone, email, status "
                "FROM employees WHERE id = ?",
                (cursor.lastrowid,),
            ).fetchone()
            self.respond(201, employee_from_row(row))

        def route(self):
            location = urlsplit(self.path).path
            if location not in ("/", "/health", "/api/employees"):
                self.respond(404, {"error": "not found"})
                return
            if location == "/api/employees":
                if self.command == "GET":
                    self.list_employees()
                elif self.command == "POST":
                    self.create_employee()
                else:
                    self.respond(405, {"error": "method not allowed"}, allow="GET, POST")
                return
            if self.command != "GET":
                self.respond(405, {"error": "method not allowed"}, allow="GET")
                return
            if location == "/":
                self.respond(200, PAGE, html=True)
            else:
                self.respond(200, {"status": "ok", "product": PRODUCT})

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
