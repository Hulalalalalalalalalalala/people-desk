#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""旧档案（仅有编号与姓名的历史员工）的端到端自动化回归测试。

PeopleDesk 允许继续使用已有业务数据：早期员工在数据库中只有 id 和 name
两列，没有工号、部门、岗位、生效日期、联系方式及任职状态。本模块在启动
服务前先把数据目录准备成这种旧表结构（只有 id/name 两列、编号不连续、
含同名旧员工），再由服务进程自行完成补列迁移，复现真实的历史业务数据，
验证读取与展示旧员工时的公开行为：

接口级（仅标准库，真实 HTTP 进程）：
1. 员工列表接口正常返回旧员工，保留原编号与姓名；缺失的档案字段
   （employee_no/department/position/effective_date/phone/email/status）
   键仍在员工对象中且值为 null——不得擅自生成工号、填默认部门、
   把生效日期补成当天，或把未知任职状态当成“在职”；
2. 列表按编号升序排列，已有编号不连续（3、8、15）也不得被重新编号；
   多名旧员工都缺少工号时各自可查，同名旧员工保留为不同编号的独立记录，
   不按姓名合并，也不因工号为空而遗漏；
3. 在同一份业务数据中新增一名资料完整、工号未使用的员工后，新旧员工
   同时可查：新员工保留建档得到的编号、“在职”状态与填写内容，旧员工
   仍保留原编号、姓名，缺失字段仍为 null，不因新增成功被删除、覆盖或
   补成新员工的资料。

界面级（Playwright 驱动本机 Chrome，headless；缺失时整类跳过）：
4. 首页把旧员工与接口记录一一对应渲染：姓名正常显示，工号、部门、岗位、
   状态、生效日期的缺失位置显示“未填写”，电话邮箱都缺时联系方式也显示
   “未填写”；页面不出现 null/undefined 字样，不把有旧员工的列表显示成
   “还没有员工记录”，缺失提示只替代对应资料，不遮姓名、不让列错位；
5. 页面上提交资料完整的新员工后，旧员工的缺失提示与新员工的真实信息
   同时呈现；接口中的缺失值仍为 null，“未填写”只是页面提示，不作为
   档案内容保存。

运行方式：

    python3 test_legacy_employee_records.py            # 接口级用例
    .venv/bin/python test_legacy_employee_records.py   # 含界面级用例（需 Playwright）

工号唯一性、生效日期校验与新建档案表单的回归保障继续保留在
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

# 旧档案：早期数据只有 id 与 name，编号刻意不连续（3、8、15），
# 其中两条同名，用于验证不按姓名合并、不因工号为空而遗漏。
LEGACY_ROWS = (
    (3, "旧档案·王五"),
    (8, "旧档案·王五"),
    (15, "旧档案·刘六"),
)
LEGACY_IDS = [row[0] for row in LEGACY_ROWS]

# 员工对象应包含的全部键：编号、姓名 + 旧数据中可能缺失的档案字段。
PROFILE_FIELDS = (
    "employee_no", "department", "position",
    "effective_date", "phone", "email", "status",
)
EMPLOYEE_KEYS = ("id", "name") + PROFILE_FIELDS

# 新增的资料完整员工：工号未被使用，内容与旧档案明显不同，
# 一旦旧档案被覆盖或补成新员工资料，断言会立刻暴露。
NEW_EMPLOYEE = {
    "employee_no": "N100",
    "name": "新建档案·陈七",
    "department": "研发部",
    "position": "测试工程师",
    "effective_date": "2026-10-01",
    "phone": "13800001000",
    "email": "new.n100@example.com",
}

FORM_FIELDS = (
    "employee_no", "name", "department", "position",
    "effective_date", "phone", "email",
)


def seed_legacy_data(tmpdir):
    """把数据目录准备成旧版业务数据：只有 id/name 两列的表和旧员工记录。

    服务启动时会自行 CREATE/ALTER 补齐后续列，这里刻意保持旧表结构，
    复现真实的历史业务数据迁移路径。
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
    """以含旧档案的数据目录启动真实服务进程，返回 (base_url, proc, tmpdir)。"""
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
                stop_server(proc, tmpdir)
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


class LegacyRecordsApiTest(unittest.TestCase):
    """接口级：旧档案的读取、排序、编号保留与新旧共存。"""

    @classmethod
    def setUpClass(cls):
        cls.base_url, cls.proc, cls.tmpdir = start_server()

    @classmethod
    def tearDownClass(cls):
        stop_server(cls.proc, cls.tmpdir)

    def test_legacy_records_read_and_coexist_with_new_employee(self):
        """旧员工正常返回且字段为 null → 新增完整员工 → 新旧同时可查且互不干扰。"""

        # 1. 列表接口正常返回旧员工：不拒绝记录，按原编号升序，不重新编号。
        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["id"] for row in employees],
            LEGACY_IDS,
            "旧员工应按原编号升序返回，编号不连续也不得重新编号",
        )
        self.assertEqual(len(employees), len(LEGACY_ROWS), "旧员工一条都不能遗漏")

        # 2. 每条旧记录：保留原编号与姓名；缺失的档案字段键仍在、值为 null。
        legacy_by_id = {}
        for legacy_id, legacy_name in LEGACY_ROWS:
            matches = [row for row in employees if row["id"] == legacy_id]
            self.assertEqual(
                len(matches), 1, f"编号 {legacy_id} 的旧员工应恰好出现一次"
            )
            record = matches[0]
            legacy_by_id[legacy_id] = record
            self.assertEqual(
                set(record), set(EMPLOYEE_KEYS),
                "缺失的档案字段仍应作为键出现在员工对象中",
            )
            self.assertEqual(record["name"], legacy_name, "旧员工姓名必须原样保留")
            for field in PROFILE_FIELDS:
                self.assertIsNone(
                    record[field],
                    f"旧员工（id={legacy_id}）缺失的 {field} 应为 null："
                    "不得擅自生成工号、填默认部门、把生效日期补成当天，"
                    "或把未知任职状态当成“在职”",
                )

        # 3. 同名旧员工保留为不同编号的独立记录：不按姓名合并，
        #    也不因工号都为空而遗漏其中一条。
        same_name = [row for row in employees if row["name"] == "旧档案·王五"]
        self.assertEqual(
            sorted(row["id"] for row in same_name), [3, 8],
            "同名旧员工必须保留为不同编号的独立记录",
        )
        self.assertTrue(
            all(row["employee_no"] is None for row in same_name),
            "多名旧员工都缺少工号时仍应各自可查",
        )

        # 4. 同一份业务数据中新增一名资料完整、工号未被使用的员工。
        status, created = request(self.base_url, "POST", "/api/employees", NEW_EMPLOYEE)
        self.assertEqual(status, 201, f"含旧档案的数据中应能正常新增员工: {created}")
        new_id = created["id"]
        self.assertIsInstance(new_id, int)
        self.assertNotIn(new_id, LEGACY_IDS, "新员工编号不得与旧员工冲突")
        self.assertEqual(created["employee_no"], NEW_EMPLOYEE["employee_no"])
        self.assertEqual(created["name"], NEW_EMPLOYEE["name"])
        self.assertEqual(created["department"], NEW_EMPLOYEE["department"])
        self.assertEqual(created["position"], NEW_EMPLOYEE["position"])
        self.assertEqual(created["effective_date"], NEW_EMPLOYEE["effective_date"])
        self.assertEqual(created["phone"], NEW_EMPLOYEE["phone"])
        self.assertEqual(created["email"], NEW_EMPLOYEE["email"])
        self.assertEqual(created["status"], "在职", "新建员工的任职状态固定为在职")

        # 5. 新旧员工同时可查：列表仍按编号升序，旧员工逐字段保持原样，
        #    不因新增成功被删除、覆盖或补成新员工的资料。
        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["id"] for row in employees],
            LEGACY_IDS + [new_id],
            "新增后新旧员工应同时可查，顺序仍按编号升序",
        )
        for legacy_id in LEGACY_IDS:
            record = next(row for row in employees if row["id"] == legacy_id)
            self.assertEqual(
                record, legacy_by_id[legacy_id],
                f"旧员工（id={legacy_id}）在新增后必须保持原样，缺失字段仍为 null",
            )
            self.assertNotEqual(record["name"], NEW_EMPLOYEE["name"])
        fetched_new = next(row for row in employees if row["id"] == new_id)
        self.assertEqual(fetched_new, created, "新员工档案应与建档响应逐字段一致")

        # 6. 旧员工的空工号不得被视为“已占用”：再新增一名不同工号的员工仍成功。
        another = dict(NEW_EMPLOYEE, employee_no="N101", name="新建档案·林八",
                       email="new.n101@example.com")
        status, created2 = request(self.base_url, "POST", "/api/employees", another)
        self.assertEqual(status, 201, f"旧员工空工号不得妨碍后续建档: {created2}")
        self.assertEqual(created2["employee_no"], "N101")


# ---------------------------------------------------------------------------
# 界面级：首页对旧档案的渲染（需要 Playwright 与本机 Chrome）
# ---------------------------------------------------------------------------

_pw = None
_browser = None


def _launch_browser(playwright):
    """优先使用本机 Chrome；不可用时回退到 Playwright 自带的 Chromium。"""
    try:
        return playwright.chromium.launch(channel="chrome", headless=True)
    except Exception:
        return playwright.chromium.launch(headless=True)


def _ui_set_up_module():
    global _pw, _browser
    if sync_playwright is None:
        return
    _pw = sync_playwright().start()
    _browser = _launch_browser(_pw)


def _ui_tear_down_module():
    global _pw, _browser
    if _browser is not None:
        _browser.close()
        _browser = None
    if _pw is not None:
        _pw.stop()
        _pw = None


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class LegacyRecordsHomepageTest(unittest.TestCase):
    """首页把旧员工与接口记录一一对应渲染，缺失位置显示“未填写”。"""

    @classmethod
    def setUpClass(cls):
        _ui_set_up_module()
        cls.base_url, cls.proc, cls.tmpdir = start_server()

    @classmethod
    def tearDownClass(cls):
        stop_server(cls.proc, cls.tmpdir)
        _ui_tear_down_module()

    def setUp(self):
        self.context = _browser.new_context()
        self.page = self.context.new_page()
        self.addCleanup(self.context.close)

    def open_homepage(self):
        """打开首页并等待首次列表加载完成（出现表格或空态提示）。"""
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

    def test_legacy_rows_render_with_placeholders(self):
        """旧员工：姓名正常显示，缺失字段显示“未填写”，不出现 null/undefined。"""
        self.open_homepage()

        # 有旧员工时绝不能显示空态提示，列表必须成功渲染成表格。
        self.assertNotIn("还没有员工记录", self.list_text())
        self.assertNotIn("加载失败", self.list_text())
        self.assertIsNotNone(
            self.page.query_selector("#list table"), "旧员工列表应渲染成表格"
        )

        rows = self.list_rows()
        self.assertEqual(len(rows), len(LEGACY_ROWS), "旧员工一条都不能遗漏")

        # 页面行与接口记录一一对应（顺序、内容一致）。
        employees = list_employees(self.base_url)
        self.assertEqual(rows, [expected_row(row) for row in employees])

        for row in rows:
            # 每行恰好 7 个单元格：缺失提示只替代对应资料，不让后面的列错位。
            self.assertEqual(len(row), 7, f"列数错位: {row!r}")
            # 工号、部门、岗位、状态、生效日期、联系方式均显示“未填写”。
            for index in (0, 2, 3, 4, 5, 6):
                self.assertEqual(row[index], "未填写")
        # 姓名正常显示，不被缺失提示遮掉；同名旧员工是两行独立记录。
        self.assertEqual(
            [row[1] for row in rows],
            [name for _, name in LEGACY_ROWS],
        )
        self.assertEqual([rows[0][1], rows[1][1]], ["旧档案·王五", "旧档案·王五"])

        # 页面不能把 null 或 undefined 当作员工资料展示。
        page_text = self.list_text()
        self.assertNotIn("null", page_text)
        self.assertNotIn("undefined", page_text)

    def test_new_employee_coexists_with_legacy_placeholders(self):
        """页面提交完整新员工后：旧员工缺失提示与新员工真实信息同时呈现。"""
        self.open_homepage()

        # 通过首页表单新增资料完整、工号未使用的员工。
        for name, value in NEW_EMPLOYEE.items():
            self.page.fill(f"#emp-form input[name={name}]", value)
        self.page.click("#emp-form button[type=submit]")
        self.page.wait_for_function(
            """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
            arg=NEW_EMPLOYEE["employee_no"],
            timeout=10000,
        )

        # 接口侧：新旧员工同时可查，旧员工缺失字段仍为 null。
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), len(LEGACY_ROWS) + 1)
        legacy = [row for row in employees if row["id"] in LEGACY_IDS]
        self.assertEqual(len(legacy), len(LEGACY_ROWS), "旧员工不得因新增被删除")
        for record in legacy:
            for field in PROFILE_FIELDS:
                self.assertIsNone(
                    record[field],
                    f"新增后旧员工（id={record['id']}）的 {field} 仍应为 null",
                )
        created = next(row for row in employees if row["id"] not in LEGACY_IDS)
        self.assertEqual(created["employee_no"], NEW_EMPLOYEE["employee_no"])
        self.assertEqual(created["status"], "在职")

        # “未填写”只是页面提示，不能作为档案内容保存。
        for record in employees:
            for field in EMPLOYEE_KEYS:
                self.assertNotEqual(
                    record[field], "未填写",
                    "页面提示“未填写”不得写入档案内容",
                )

        # 页面同时呈现旧员工的缺失提示与新员工的真实信息，且与接口逐行对应。
        rows = self.list_rows()
        self.assertEqual(rows, [expected_row(row) for row in employees])
        legacy_rows, new_row = rows[: len(LEGACY_ROWS)], rows[-1]
        for row in legacy_rows:
            self.assertEqual(len(row), 7)
            self.assertEqual(row[0], "未填写", "旧员工工号位置仍显示未填写")
            self.assertIn(row[1], [name for _, name in LEGACY_ROWS])
        self.assertEqual(
            new_row,
            [NEW_EMPLOYEE["employee_no"], NEW_EMPLOYEE["name"],
             NEW_EMPLOYEE["department"], NEW_EMPLOYEE["position"],
             "在职", NEW_EMPLOYEE["effective_date"],
             f"{NEW_EMPLOYEE['phone']} · {NEW_EMPLOYEE['email']}"],
            "新员工应显示真实填写内容与在职状态",
        )
        page_text = self.list_text()
        self.assertNotIn("null", page_text)
        self.assertNotIn("undefined", page_text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
