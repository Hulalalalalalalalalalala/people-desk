#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""旧档案（仅有编号和姓名的早期员工）的端到端自动化回归测试。

PeopleDesk 允许继续使用已有业务数据：早期员工在数据库里只保存了 id 和
name，没有工号、部门、岗位、生效日期、联系方式及任职状态。本模块在服务
启动前把这样的旧库写入数据目录，再用真实服务进程验证：

接口部分（仅使用标准库，任何环境都可执行）：
1. 员工列表接口正常返回旧员工：保留原编号（不连续也不重新编号）和姓名，
   缺失的档案字段仍以键出现在员工对象中、值为 null；不得擅自生成工号、
   填默认部门、把生效日期补成当天或把未知任职状态当成“在职”；
2. 多名旧员工都缺少工号时各自可查；姓名相同的旧员工保留为不同编号的
   独立记录，不按姓名合并，也不因工号为空而遗漏；
3. 在同一份业务数据中新增一名资料完整、工号未使用的员工后，新旧员工
   同时可查：新员工保留正常建档得到的编号、在职状态与填写内容，旧员工
   仍保留原编号、姓名和 null 字段，不被删除、覆盖或补成新员工的资料。

界面部分（需要 Playwright 与本机 Chrome，缺省时自动跳过）：
4. 首页把旧员工与接口记录一一对应渲染：姓名正常显示，工号、部门、岗位、
   状态、生效日期的缺失位置显示“未填写”，电话邮箱都缺时联系方式显示
   “未填写”；页面不展示 null/undefined，不把有旧员工的列表显示成
   “还没有员工记录”，缺失提示只替代对应资料、不遮姓名、不让后面的列错位；
5. 通过首页表单新增资料完整的员工后，页面同时呈现旧员工的“未填写”提示
   与新员工的真实信息；接口中的缺失值仍为 null，“未填写”只是页面提示，
   不作为档案内容保存。

运行方式：

    python3 test_legacy_employee_records.py            # 接口回归始终执行
    .venv/bin/python test_legacy_employee_records.py   # 含界面回归（需 Playwright）

工号唯一性、生效日期与首页表单的既有回归继续保留在
test_employee_no_uniqueness.py、test_effective_date_validation.py 与
test_homepage_employee_form.py，本模块不改变任何建档规则。
"""
import json
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

try:
    from playwright.sync_api import sync_playwright
except ImportError:  # pragma: no cover - 环境缺少 playwright 时界面用例整类跳过
    sync_playwright = None

APP_PATH = Path(__file__).resolve().parent / "app.py"

# 旧档案：早期数据只有编号和姓名。编号刻意不连续（3、8、12），
# 其中两条姓名相同，用于验证不按姓名合并、不因工号为空而遗漏。
LEGACY_ROWS = (
    (3, "旧档案·陈甲"),
    (8, "旧档案·王乙"),
    (12, "旧档案·陈甲"),
)
LEGACY_IDS = [row[0] for row in LEGACY_ROWS]

# 旧员工缺失的全部档案字段：接口中必须出现且值为 null。
ARCHIVE_FIELDS = (
    "employee_no", "department", "position",
    "effective_date", "phone", "email", "status",
)

# 新增的资料完整员工：工号未被使用，内容与旧员工明显不同，
# 一旦旧记录被覆盖或补成新员工资料，断言会立刻暴露。
NEW_EMPLOYEE = {
    "employee_no": "N501",
    "name": "新建档案·林晚",
    "department": "研发部",
    "position": "前端工程师",
    "effective_date": "2026-10-03",
    "phone": "13755550001",
    "email": "new.n501@example.com",
}

FORM_FIELDS = (
    "employee_no", "name", "department", "position",
    "effective_date", "phone", "email",
)

# 页面表头顺序：工号/姓名/部门/岗位/状态/生效日期/联系方式。
LIST_COLUMN_COUNT = 7

_pw = None
_browser = None


def _launch_browser(playwright):
    """优先使用本机 Chrome；不可用时回退到 Playwright 自带的 Chromium。"""
    try:
        return playwright.chromium.launch(channel="chrome", headless=True)
    except Exception:
        return playwright.chromium.launch(headless=True)


def setUpModule():
    global _pw, _browser
    if sync_playwright is None:
        return
    _pw = sync_playwright().start()
    _browser = _launch_browser(_pw)


def tearDownModule():
    global _pw, _browser
    if _browser is not None:
        _browser.close()
        _browser = None
    if _pw is not None:
        _pw.stop()
        _pw = None


def seed_legacy_data(tmpdir):
    """在服务启动前写入旧版业务库：employees 表只有 id 和 name 两列。

    服务启动时会按现有逻辑为旧表补齐后续列（值为 NULL），
    与真实环境沿用旧业务数据的路径一致。
    """
    database = sqlite3.connect(Path(tmpdir) / "people-desk.sqlite")
    database.execute(
        "CREATE TABLE employees (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"
    )
    database.executemany(
        "INSERT INTO employees (id, name) VALUES (?, ?)", LEGACY_ROWS
    )
    database.commit()
    database.close()


def start_server():
    """以预置旧档案的数据目录启动真实服务进程，返回 (base_url, proc, tmpdir)。"""
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-legacy-test-")
    seed_legacy_data(tmpdir)
    proc = subprocess.Popen(
        [
            sys.executable,
            str(APP_PATH),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--data-dir",
            tmpdir,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    deadline = time.time() + 10
    line = ""
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            if proc.poll() is not None:
                raise RuntimeError("服务进程提前退出，未能启动")
            time.sleep(0.05)
            continue
        match = re.search(r"http://[^/\s]+:(\d+)", line)
        if match:
            return f"http://127.0.0.1:{match.group(1)}", proc, tmpdir
    stop_server(proc, tmpdir)
    raise RuntimeError(f"未能从服务输出解析监听地址: {line!r}")


def stop_server(proc, tmpdir):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        if proc.stdout is not None:
            proc.stdout.close()
    if tmpdir is not None:
        shutil.rmtree(tmpdir, ignore_errors=True)


def request(base_url, method, path, body=None):
    """发起 HTTP 请求，返回 (status_code, json_body)。"""
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body, ensure_ascii=False).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        base_url + path, data=data, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
            return resp.status, payload
    except urllib.error.HTTPError as err:
        payload = json.loads(err.read().decode("utf-8"))
        return err.code, payload


def list_employees(base_url):
    """查询员工列表接口，返回按 id 升序的记录列表。"""
    status, body = request(base_url, "GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    return body["employees"]


def expected_row(employee):
    """按首页渲染规则，把一条接口记录折算为列表中应显示的一行。"""
    def text(value):
        return "未填写" if value in (None, "") else value

    parts = [value for value in (employee.get("phone"), employee.get("email")) if value]
    contact = " · ".join(parts) if parts else "未填写"
    return [
        text(employee.get("employee_no")),
        employee.get("name"),
        text(employee.get("department")),
        text(employee.get("position")),
        text(employee.get("status")),
        text(employee.get("effective_date")),
        contact,
    ]


class LegacyServerCase(unittest.TestCase):
    """每个用例使用独立的预置旧档案服务进程与数据目录，用例间互不影响。"""

    def setUp(self):
        self.base_url, self.proc, self.tmpdir = start_server()
        self.addCleanup(stop_server, self.proc, self.tmpdir)

    def assert_legacy_record(self, record, legacy_id, name):
        """核对一条旧员工记录：原编号、原姓名，档案字段齐全且全为 null。"""
        self.assertEqual(record["id"], legacy_id, "旧员工编号必须保持原值")
        self.assertEqual(record["name"], name, "旧员工姓名必须保持原值")
        for field in ARCHIVE_FIELDS:
            self.assertIn(field, record, f"缺失的档案字段 {field} 仍应出现在员工对象中")
            self.assertIsNone(
                record[field],
                f"旧员工的 {field} 应为 null，不得擅自补值"
                f"（不得生成工号、填默认部门、补当天日期或当作在职）",
            )


class LegacyArchiveApiTest(LegacyServerCase):
    """旧档案在员工列表接口中的读取与新旧共存。"""

    def test_legacy_records_returned_with_null_fields(self):
        employees = list_employees(self.base_url)

        self.assertEqual(
            [row["id"] for row in employees],
            LEGACY_IDS,
            "旧员工应全部返回、按编号升序排列，编号不连续也不得重新编号",
        )
        for (legacy_id, name), record in zip(LEGACY_ROWS, employees):
            self.assert_legacy_record(record, legacy_id, name)

        # 缺失字段不得被擅自补值的具体红线。
        for record in employees:
            self.assertNotEqual(
                record["status"], "在职", "未知任职状态不得当成“在职”"
            )
            self.assertNotEqual(
                record["effective_date"],
                "2026-10-03",
                "生效日期不得补成当天",
            )
            self.assertNotEqual(
                record["department"], "研发部", "不得填入默认部门"
            )

    def test_same_name_legacy_records_stay_separate(self):
        employees = list_employees(self.base_url)
        same_name = [row for row in employees if row["name"] == "旧档案·陈甲"]
        self.assertEqual(
            [row["id"] for row in same_name],
            [3, 12],
            "姓名相同的旧员工必须保留为不同编号的独立记录，不得按姓名合并",
        )
        self.assertEqual(
            len(employees),
            len(LEGACY_ROWS),
            "多名旧员工都缺少工号时仍各自可查，不得因工号为空而遗漏",
        )

    def test_create_full_employee_keeps_legacy_records(self):
        before = list_employees(self.base_url)
        self.assertEqual([row["id"] for row in before], LEGACY_IDS)

        # 新增一名资料完整、工号未被使用的员工。
        status, created = request(self.base_url, "POST", "/api/employees", NEW_EMPLOYEE)
        self.assertEqual(status, 201, f"旧数据存在时新建完整档案应成功: {created}")
        new_id = created["id"]
        self.assertIsInstance(new_id, int)
        self.assertNotIn(new_id, LEGACY_IDS, "新员工编号不得复用或覆盖旧编号")
        self.assertGreater(new_id, max(LEGACY_IDS), "已有编号不连续也不得重新编号")

        # 新员工保留正常建档得到的填写内容与在职状态。
        self.assertEqual(created["employee_no"], NEW_EMPLOYEE["employee_no"])
        self.assertEqual(created["name"], NEW_EMPLOYEE["name"])
        self.assertEqual(created["department"], NEW_EMPLOYEE["department"])
        self.assertEqual(created["position"], NEW_EMPLOYEE["position"])
        self.assertEqual(created["effective_date"], NEW_EMPLOYEE["effective_date"])
        self.assertEqual(created["phone"], NEW_EMPLOYEE["phone"])
        self.assertEqual(created["email"], NEW_EMPLOYEE["email"])
        self.assertEqual(created["status"], "在职")

        # 新旧员工同时可查：旧员工保持原编号、原姓名与 null 字段，
        # 不因新增成功被删除、覆盖或补成新员工的资料。
        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["id"] for row in employees],
            LEGACY_IDS + [new_id],
            "新增后列表仍按编号升序，旧员工一条不少",
        )
        self.assertEqual(
            employees[: len(LEGACY_IDS)],
            before,
            "旧员工记录必须逐字段保持原样",
        )
        for (legacy_id, name), record in zip(LEGACY_ROWS, employees):
            self.assert_legacy_record(record, legacy_id, name)
            self.assertNotEqual(record["name"], NEW_EMPLOYEE["name"])
        self.assertEqual(employees[-1], created, "新员工查询结果应与建档响应一致")


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class LegacyArchiveHomepageTest(LegacyServerCase):
    """旧档案在首页的渲染，以及页面新增后新旧员工的同屏呈现。"""

    def setUp(self):
        super().setUp()
        self.context = _browser.new_context()
        self.page = self.context.new_page()
        self.addCleanup(self.context.close)

    def open_homepage(self):
        self.page.goto(self.base_url + "/")
        self.page.wait_for_selector("#list table, #list .empty", timeout=10000)

    def list_rows(self):
        """读取首页员工列表当前渲染出的全部数据行（文本与页面显示一致）。"""
        return self.page.evaluate(
            """() => Array.from(
                document.querySelectorAll('#list tbody tr'),
                (tr) => Array.from(tr.cells, (td) => td.textContent)
            )"""
        )

    def list_text(self):
        return self.page.evaluate(
            "() => document.getElementById('list').textContent"
        )

    def test_homepage_shows_placeholder_for_missing_fields(self):
        self.open_homepage()

        text = self.list_text()
        self.assertNotIn("还没有员工记录", text, "有旧员工时不得显示空态提示")
        self.assertNotIn("加载失败", text, "某一项缺失不得让整张列表加载失败")
        for forbidden in ("null", "undefined", "None"):
            self.assertNotIn(
                forbidden, text, f"页面不得把 {forbidden} 当作员工资料展示"
            )

        employees = list_employees(self.base_url)
        rows = self.list_rows()
        self.assertEqual(
            rows,
            [expected_row(employee) for employee in employees],
            "首页旧员工应与接口记录一一对应",
        )
        self.assertEqual(len(rows), len(LEGACY_ROWS))

        for (legacy_id, name), row in zip(LEGACY_ROWS, rows):
            self.assertEqual(
                len(row), LIST_COLUMN_COUNT, "缺失提示不得让后面的列错位"
            )
            self.assertEqual(
                row,
                ["未填写", name, "未填写", "未填写", "未填写", "未填写", "未填写"],
                "工号/部门/岗位/状态/生效日期/联系方式缺失时应显示“未填写”",
            )
            self.assertEqual(row[1], name, "缺失提示只替代对应资料，不得遮掉姓名")

    def test_create_via_form_coexists_with_legacy(self):
        self.open_homepage()
        self.assertEqual(len(self.list_rows()), len(LEGACY_ROWS))

        # 通过首页表单新增一名资料完整、工号未被使用的员工。
        for field, value in NEW_EMPLOYEE.items():
            self.page.fill(f"#emp-form input[name={field}]", value)
        self.page.click("#emp-form button[type=submit]")
        self.page.wait_for_function(
            """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
            arg=NEW_EMPLOYEE["employee_no"],
            timeout=10000,
        )

        # 页面同时呈现旧员工的缺失提示与新员工的真实信息，且与接口一一对应。
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), len(LEGACY_ROWS) + 1)
        rows = self.list_rows()
        self.assertEqual(
            rows,
            [expected_row(employee) for employee in employees],
            "页面列表必须与员工列表接口查询结果一一对应",
        )
        for row in rows[: len(LEGACY_ROWS)]:
            self.assertEqual(len(row), LIST_COLUMN_COUNT)
            self.assertEqual(row[0], "未填写", "旧员工工号仍应显示“未填写”")
            self.assertEqual(row[6], "未填写", "旧员工联系方式仍应显示“未填写”")
        new_row = rows[-1]
        self.assertEqual(
            new_row,
            [
                NEW_EMPLOYEE["employee_no"],
                NEW_EMPLOYEE["name"],
                NEW_EMPLOYEE["department"],
                NEW_EMPLOYEE["position"],
                "在职",
                NEW_EMPLOYEE["effective_date"],
                f"{NEW_EMPLOYEE['phone']} · {NEW_EMPLOYEE['email']}",
            ],
            "新员工应显示真实填写内容与在职状态",
        )

        # “未填写”只是页面提示：接口中的缺失值仍为 null，旧员工保持原样。
        for (legacy_id, name), record in zip(LEGACY_ROWS, employees):
            self.assert_legacy_record(record, legacy_id, name)
        self.assertNotIn(
            "未填写",
            json.dumps(employees, ensure_ascii=False),
            "“未填写”不得作为档案内容保存",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
