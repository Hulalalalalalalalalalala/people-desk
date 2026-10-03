#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""首页“新建员工档案”的端到端界面回归测试。

与接口级测试（工号唯一性、生效日期校验）不同，本模块通过真实浏览器
（Playwright 驱动本机 Chrome，headless）在首页实际填写表单并点击
“保存档案”，验证提交后页面侧的行为，而不是把接口返回成功当作页面
已经正确展示的依据：

1. 合法档案提交成功后：员工列表无需手动刷新整页即出现刚保存的员工；
   工号、姓名、部门、岗位、生效日期、电话、邮箱全部恢复为空；错误提示
   不显示。列表中的工号与必填文字为去掉首尾空白后的保存值，生效日期
   与提交值一致，状态为“在职”，电话与邮箱出现在该员工的联系方式中；
   刚创建的记录与此前已存在的记录都能逐字段区分核对。
2. 已有工号“A007”时，用“ a007 ”提交另一份完整且日期合法的档案：
   页面显示明确说明工号已存在的中文提示；刚填写的所有内容（含首尾空白
   与选填联系方式）原样保留在表单中；员工列表仍只有原档案，既不新增
   失败提交的员工，也不把原员工的姓名、部门或联系方式替换成再次提交的
   内容（再次提交使用与原档案明显不同的填写内容，覆盖必被发现）。
3. 只把失败表单中的工号改为未使用的工号再次保存：正常新增为另一名
   员工，原档案保持原样，之前的错误提示消失，表单清空；最终页面列表
   与员工列表接口查询结果逐行对应，既不会显示未保存的员工，也不会在
   记录已保存后停留在错误状态。

运行方式（需要 Playwright 与本机 Chrome）：

    .venv/bin/python test_homepage_employee_form.py

工号唯一性与生效日期的接口级回归测试继续保留在
test_employee_no_uniqueness.py 与 test_effective_date_validation.py，
本模块不改变任何建档规则。
"""
import json
import re
import shutil
import signal
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
except ImportError:  # pragma: no cover - 环境缺少 playwright 时整模块跳过
    sync_playwright = None

APP_PATH = Path(__file__).resolve().parent / "app.py"

FORM_FIELDS = (
    "employee_no", "name", "department", "position",
    "effective_date", "phone", "email",
)

# 页面表头顺序：工号/姓名/部门/岗位/状态/生效日期/联系方式。
LIST_COLUMNS = (
    "employee_no", "name", "department", "position",
    "status", "effective_date", "contact",
)

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


def start_server():
    """以全新数据目录启动真实服务进程，返回 (base_url, proc, tmpdir)。"""
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-ui-test-")
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


def fill_form(page, values):
    """按输入框 name 逐个填写，值原样送入（含首尾空白），与用户键入一致。"""
    for name, value in values.items():
        page.fill(f"#emp-form input[name={name}]", value)


def read_form(page):
    """读取表单当前所有字段的值（不做任何加工，用于核对输入保留/清空）。"""
    return page.evaluate(
        """(fields) => {
            const form = document.getElementById('emp-form');
            const out = {};
            for (const name of fields) {
                out[name] = form.elements.namedItem(name).value;
            }
            return out;
        }""",
        list(FORM_FIELDS),
    )


def error_state(page):
    """返回错误提示的 {hidden, text}。"""
    return page.evaluate(
        """() => {
            const box = document.getElementById('form-error');
            return {hidden: box.hidden, text: box.textContent};
        }"""
    )


def list_rows(page):
    """读取首页员工列表当前渲染出的全部数据行（文本与页面显示一致）。"""
    return page.evaluate(
        """() => Array.from(
            document.querySelectorAll('#list tbody tr'),
            (tr) => Array.from(tr.cells, (td) => td.textContent)
        )"""
    )


def open_homepage(page, base_url):
    """打开首页并等待首次列表加载完成（出现表格或空态提示）。"""
    page.goto(base_url + "/")
    page.wait_for_selector("#list table, #list .empty", timeout=10000)


def submit_form(page):
    page.click("#emp-form button[type=submit]")


def wait_error_visible(page):
    page.wait_for_selector("#form-error:not([hidden])", timeout=10000)


def wait_list_has_employee_no(page, employee_no):
    """等待列表第一列出现指定工号（提交成功后列表异步刷新的完成信号）。"""
    page.wait_for_function(
        """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
            .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
        arg=employee_no,
        timeout=10000,
    )


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class HomepageCase(unittest.TestCase):
    """每个用例类使用独立的服务进程与数据目录，用例间互不影响。"""

    @classmethod
    def setUpClass(cls):
        cls.base_url, cls.proc, cls.tmpdir = start_server()

    @classmethod
    def tearDownClass(cls):
        stop_server(cls.proc, cls.tmpdir)

    def setUp(self):
        self.context = _browser.new_context()
        self.page = self.context.new_page()
        self.addCleanup(self.context.close)

    def assert_error_hidden(self):
        state = error_state(self.page)
        self.assertTrue(
            state["hidden"],
            f"错误提示不应显示，当前文本: {state['text']!r}",
        )

    def assert_form_values(self, expected, message):
        self.assertEqual(read_form(self.page), dict(expected), message)

    def assert_form_cleared(self):
        self.assert_form_values(
            {name: "" for name in FORM_FIELDS},
            "提交成功后表单所有字段都应恢复为空",
        )


class SuccessfulSubmissionTest(HomepageCase):
    """合法档案提交成功后的列表刷新与表单清空。"""

    # 已有记录与新建记录使用完全不同的内容，确保两者能被明确区分。
    BASELINE = {
        "employee_no": "E200",
        "name": "已有档案·苏基",
        "department": "财务部",
        "position": "会计",
        "effective_date": "2026-08-15",
        "phone": "13600000000",
        "email": "baseline.e200@example.com",
    }

    # 新建档案的文本字段刻意带首尾空白：列表必须显示去掉空白后的保存值，
    # 而表单提交时送出的仍是用户原始输入。
    NEW = {
        "employee_no": " E201 ",
        "name": " 首页建档·林一 ",
        "department": " 产品部 ",
        "position": " 视觉设计师 ",
        "effective_date": "2026-10-01",
        "phone": " 13711112222 ",
        "email": " home.e201@example.com ",
    }

    def test_successful_submission_refreshes_list_and_clears_form(self):
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline}")

        open_homepage(self.page, self.base_url)
        self.assertEqual(
            list_rows(self.page),
            [expected_row(baseline)],
            "提交前列表应只有此前已存在的基线员工",
        )
        self.assert_error_hidden()

        # 在页面里打标记：若提交后发生整页刷新，标记会丢失，
        # 借此证明列表更新来自页内异步刷新而非用户手动刷新整页。
        self.page.evaluate("() => { window.__stayOnSamePage = 'yes'; }")

        fill_form(self.page, self.NEW)
        submit_form(self.page)

        # 不刷新页面，等待刚保存的员工出现在列表中。
        wait_list_has_employee_no(self.page, "E201")
        self.assertEqual(
            self.page.evaluate("() => window.__stayOnSamePage"),
            "yes",
            "列表更新不应伴随整页刷新",
        )

        # 表单全部字段恢复为空，错误提示不显示。
        self.assert_form_cleared()
        self.assert_error_hidden()

        # 列表同时包含基线员工与新员工，顺序与接口一致（按 id 升序）。
        employees = list_employees(self.base_url)
        self.assertEqual([row["employee_no"] for row in employees], ["E200", "E201"])
        created = employees[1]

        # 接口侧保存值：必填文字去掉首尾空白，日期与提交值一致，状态在职。
        self.assertEqual(created["employee_no"], "E201")
        self.assertEqual(created["name"], "首页建档·林一")
        self.assertEqual(created["department"], "产品部")
        self.assertEqual(created["position"], "视觉设计师")
        self.assertEqual(created["effective_date"], "2026-10-01")
        self.assertEqual(created["phone"], "13711112222")
        self.assertEqual(created["email"], "home.e201@example.com")
        self.assertEqual(created["status"], "在职")
        self.assertEqual(employees[0], baseline, "基线员工档案必须保持原样")

        # 页面列表与接口查询结果逐行对应：新行显示去空白后的保存值、
        # 提交的生效日期、“在职”状态，电话与邮箱出现在联系方式中。
        rows = list_rows(self.page)
        self.assertEqual(rows, [expected_row(employees[0]), expected_row(created)])
        new_row = rows[1]
        self.assertEqual(
            new_row,
            ["E201", "首页建档·林一", "产品部", "视觉设计师",
             "在职", "2026-10-01", "13711112222 · home.e201@example.com"],
        )
        self.assertNotEqual(new_row[1], baseline["name"], "新旧记录必须可区分")
        self.assertNotEqual(new_row[2], baseline["department"], "新旧记录必须可区分")


class DuplicateThenRetryTest(HomepageCase):
    """重复工号被拒后的提示、输入保留，以及仅改工号后的成功重试。"""

    # 原档案与再次提交的档案使用明显不同的姓名/部门/岗位/联系方式，
    # 一旦发生覆盖，断言会立刻暴露。
    ORIGINAL = {
        "employee_no": "A007",
        "name": "原始档案·赵甲",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-09-01",
        "phone": "13800000001",
        "email": "original.a007@example.com",
    }

    # 工号刻意写成“ a007 ”（忽略大小写且去空白后与原档案冲突）；
    # 选填联系方式也带首尾空白，用于验证失败后的原样保留。
    DUPLICATE = {
        "employee_no": " a007 ",
        "name": "再次提交·钱乙",
        "department": "市场部",
        "position": "市场专员",
        "effective_date": "2026-10-02",
        "phone": " 13900000002 ",
        "email": " again.a007@example.com ",
    }

    RETRY_NO = "A071"

    def test_duplicate_keeps_inputs_then_retry_with_new_no_succeeds(self):
        status, original = request(self.base_url, "POST", "/api/employees", self.ORIGINAL)
        self.assertEqual(status, 201, f"原档案应建档成功: {original}")
        self.assertEqual(original["employee_no"], "A007")

        open_homepage(self.page, self.base_url)
        self.assertEqual(list_rows(self.page), [expected_row(original)])

        # ---- 第一步：用“ a007 ”再次提交完整且日期合法的档案，应被拒绝 ----
        fill_form(self.page, self.DUPLICATE)
        submit_form(self.page)
        wait_error_visible(self.page)

        state = error_state(self.page)
        self.assertFalse(state["hidden"], "工号重复时错误提示必须显示")
        self.assertIn("工号", state["text"])
        self.assertIn("已存在", state["text"])

        # 刚填写的所有内容原样保留在表单中：含首尾空白与选填联系方式。
        self.assert_form_values(
            self.DUPLICATE,
            "提交失败后用户填写的所有内容（含首尾空白与选填联系方式）都应保留",
        )

        # 员工列表仍只有原档案：失败提交不新增，也不覆盖原员工的
        # 姓名、部门或联系方式。
        rows = list_rows(self.page)
        self.assertEqual(rows, [expected_row(original)], "失败提交不得改变页面列表")
        self.assertNotIn(self.DUPLICATE["name"], rows[0])
        self.assertNotIn(self.DUPLICATE["department"], rows[0])
        self.assertNotIn(self.DUPLICATE["phone"].strip(), rows[0][6])
        self.assertNotIn(self.DUPLICATE["email"].strip(), rows[0][6])

        employees = list_employees(self.base_url)
        self.assertEqual(employees, [original], "失败提交不得在接口侧留下新记录")

        # ---- 第二步：只把工号改为未使用的工号，其余保留内容不动，再次保存 ----
        self.page.fill("#emp-form input[name=employee_no]", self.RETRY_NO)
        # 除工号外，表单仍保持失败时保留的内容。
        self.assert_form_values(
            {**self.DUPLICATE, "employee_no": self.RETRY_NO},
            "重试前应只修改工号，其余字段保持失败时保留的内容",
        )
        submit_form(self.page)

        wait_list_has_employee_no(self.page, self.RETRY_NO)

        # 错误提示消失，表单清空。
        self.assert_error_hidden()
        self.assert_form_cleared()

        # 接口侧：原档案保持原样，新档案按原有规则（去空白、在职）保存。
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), 2, "重试成功后应恰好新增一名员工")
        self.assertEqual(employees[0], original, "原 A007 档案必须保持原样")
        retried = employees[1]
        self.assertEqual(retried["employee_no"], self.RETRY_NO)
        self.assertEqual(retried["name"], self.DUPLICATE["name"].strip())
        self.assertEqual(retried["department"], self.DUPLICATE["department"].strip())
        self.assertEqual(retried["position"], self.DUPLICATE["position"].strip())
        self.assertEqual(retried["effective_date"], self.DUPLICATE["effective_date"])
        self.assertEqual(retried["phone"], self.DUPLICATE["phone"].strip())
        self.assertEqual(retried["email"], self.DUPLICATE["email"].strip())
        self.assertEqual(retried["status"], "在职")

        # 最终页面列表与接口查询结果逐行对应：既不显示未保存的员工，
        # 也不会在记录已保存后停留在错误状态。
        self.assertEqual(
            list_rows(self.page),
            [expected_row(employee) for employee in employees],
            "页面列表必须与员工列表接口查询结果一一对应",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
