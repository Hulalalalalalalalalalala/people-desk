#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""首页员工列表“读取失败”场景的端到端界面回归测试。

首页在两个时机会读取员工列表：打开页面时、成功保存新档案后。本模块
通过真实浏览器（Playwright 驱动本机 Chrome，headless）访问真实服务
进程，并用浏览器侧的请求拦截精确控制 GET /api/employees 的返回，把
三种必须区分的状态固化为回归保障；所有断言都落在用户实际看到的
列表内容、中文提示与表单状态上，不以建档接口返回成功代替页面表现：

1. 首次打开首页、尚未成功读到任何员工记录时列表读取失败：
   - 请求未能完成（网络中断）、返回失败状态（5xx）、返回内容不能
     解析（非法 JSON）、或解析后不是带 employees 数组的有效结果
     （缺少 employees、employees 不是数组）都属于读取失败；
   - “加载中……”提示必须结束，列表区域显示明确的中文失败提示；
   - 不得出现“还没有员工记录”，也不得留下像正常加载完成的空表格。
2. 只有有效响应确实返回空员工列表时才显示现有空记录提示；若上一次
   成功读取到的就是空列表，之后再读取失败必须改成失败提示，不再把
   空结果当成当前确定事实（即使服务端其实已有员工、只是空列表被成功
   返回过，失败后也不得“突然”显示那些从未成功读取到的员工）。
3. 页面已成功展示过员工后读取失败：保留上一次成功取得的记录，
   工号、姓名、部门等逐字段保持原样，不清除、不重复一份、不混入
   没读到的新员工；同时用中文说明当前显示的是上次读取的记录、本次
   未取得最新列表。之后在同一页面通过页面自身的读取操作重新取得
   最新列表时，按新结果展示且不再带失败提示。
4. 成功建档之后列表读取失败的区别：资料合法且工号未使用、保存已经
   成功，仅随后的列表读取失败时——表单按成功保存的规则清空，
   明确提示“档案已保存，但员工列表刷新失败”，保存错误提示不出现；
   列表区域按“是否曾成功取得过非空记录”决定保留旧记录还是显示
   加载失败；新档案尚未显示不代表保存失败——通过员工列表接口
   （浏览器拦截之外的独立 HTTP 请求）必须能查到实际保存的员工，
   原有档案逐字段保持不变。

运行方式（需要 Playwright 与本机 Chrome）：

    python3 test_homepage_list_load_failure.py

建档校验、工号唯一性与档案字段展示规则的既有回归继续保留在
test_text_field_validation.py、test_effective_date_validation.py、
test_employee_no_uniqueness.py、test_special_text_display.py、
test_legacy_employee_records.py 与 test_homepage_employee_form.py，
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
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-listfail-test-")
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
    """发起 HTTP 请求（绕过浏览器，因此不受页面侧列表拦截影响）。"""
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
            return resp.status, json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        return err.code, json.loads(err.read().decode("utf-8"))


def list_employees_api(base_url):
    """直接查询员工列表接口，返回按 id 升序的记录列表。"""
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


# ---- 列表请求的浏览器侧拦截 ---------------------------------------------

# 各失败形态：abort=请求未能完成；status500=失败状态；badjson=无法解析；
# missing=解析成功但没有 employees；notarray=employees 不是数组。
FAILURE_MODES = ("abort", "status500", "badjson", "missing", "notarray")

FAIL_RESPONSES = {
    "status500": (500, '{"error":"internal error"}'),
    "badjson": (200, "这不是有效JSON{"),
    "missing": (200, '{"ok": true}'),
    "notarray": (200, '{"employees": {"id": 1}}'),
}


class ListGate:
    """控制 GET /api/employees 的逐次返回。

    mode：默认形态（normal 放行到真实服务；其余见 FAILURE_MODES，
    以及 empty=返回 {"employees": []}）。
    schedule：按 GET 次序覆盖形态，例如 {1: "normal", 2: "abort"}
    表示第一次读取放行、第二次起按 mode 失败（或在此逐次指定）。
    POST 永远放行到真实服务：建档必须走真实后端。
    """

    def __init__(self, mode="normal", schedule=None):
        self.mode = mode
        self.schedule = dict(schedule or {})
        self.get_count = 0

    def install(self, page):
        def handle(route):
            if route.request.method == "POST":
                route.continue_()
                return
            self.get_count += 1
            mode = self.schedule.get(self.get_count, self.mode)
            if mode == "normal":
                route.continue_()
            elif mode == "abort":
                route.abort()
            elif mode == "empty":
                route.fulfill(
                    status=200,
                    content_type="application/json; charset=utf-8",
                    body='{"employees": []}',
                )
            else:
                status, body = FAIL_RESPONSES[mode]
                route.fulfill(
                    status=status,
                    content_type="application/json; charset=utf-8",
                    body=body,
                )

        page.route("**/api/employees", handle)


# ---- 页面读取辅助 ---------------------------------------------------------

def list_state(page):
    """读取列表区域当前对用户可见的完整状态。"""
    return page.evaluate(
        """() => {
            const root = document.getElementById('list');
            const err = root.querySelector('.msg.error');
            const empty = root.querySelector('.empty');
            return {
                fullText: root.textContent,
                hasTable: !!root.querySelector('table'),
                rows: Array.from(root.querySelectorAll('tbody tr')).map(
                    (tr) => Array.from(tr.cells, (td) => td.textContent)
                ),
                errorText: err ? err.textContent : null,
                emptyText: empty ? empty.textContent : null,
            };
        }"""
    )


def form_state(page):
    """读取表单全部字段值与两个提示框的状态。"""
    return page.evaluate(
        """(fields) => {
            const form = document.getElementById('emp-form');
            const values = {};
            for (const name of fields) {
                values[name] = form.elements.namedItem(name).value;
            }
            const err = document.getElementById('form-error');
            const note = document.getElementById('form-note');
            return {
                values,
                error: {hidden: err.hidden, text: err.textContent},
                note: {hidden: note.hidden, text: note.textContent},
            };
        }""",
        list(FORM_FIELDS),
    )


def fill_form(page, values):
    for name, value in values.items():
        page.fill(f"#emp-form input[name={name}]", value)


def submit_form(page):
    page.click("#emp-form button[type=submit]")


def wait_empty_prompt(page):
    """等待“还没有员工记录”空态（加载中的 .empty 不算）。"""
    page.wait_for_function(
        """() => {
            const el = document.querySelector('#list .empty');
            return !!el && el.textContent.includes('还没有员工记录');
        }""",
        timeout=10000,
    )


def wait_list_failure(page):
    """等待列表区域出现失败提示。"""
    page.wait_for_selector("#list .msg.error", timeout=10000)


def wait_form_finished(page):
    """等待提交结束：成功提示或保存错误提示任一出现。"""
    page.wait_for_selector(
        "#form-note:not([hidden]), #form-error:not([hidden])",
        timeout=10000,
    )


def trigger_page_list_reload(page):
    """调用页面自身的读取操作（与打开页面、保存成功后同一条 load 路径）。"""
    page.evaluate("() => load()")


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

    def open_homepage(self):
        self.page.goto(self.base_url + "/")

    # ---- 断言辅助 ----

    def assert_initial_load_failure(self, state):
        """从未成功读取时的失败形态：明确失败提示，无空态、无表格、无加载中。"""
        self.assertIsNotNone(state["errorText"], "列表区域必须显示失败提示")
        self.assertIn("加载失败", state["errorText"])
        self.assertNotIn(
            "上次", state["errorText"], "首次读取失败不应出现“上次记录”式说明"
        )
        self.assertNotIn("还没有员工记录", state["fullText"],
                         "读取失败不得显示“还没有员工记录”")
        self.assertFalse(state["hasTable"], "读取失败不得留下空表格")
        self.assertEqual(state["rows"], [])
        self.assertNotIn("加载中", state["fullText"], "加载提示必须已经结束")

    def assert_stale_list(self, state, expected_rows):
        """失败但有上次成功记录：旧记录逐行保留，并说明显示的是上次记录。"""
        self.assertTrue(state["hasTable"], "应继续展示上次成功取得的记录表格")
        self.assertEqual(state["rows"], expected_rows,
                         "列表必须原样保留上次成功读取的记录，不得清除、重复或混入新员工")
        self.assertIsNotNone(state["errorText"], "必须说明本次未能取得最新列表")
        self.assertIn("加载失败", state["errorText"])
        self.assertIn("上次读取", state["errorText"])
        self.assertIn("最新列表", state["errorText"])
        self.assertNotIn("加载中", state["fullText"])
        self.assertNotIn("还没有员工记录", state["fullText"])


class InitialLoadFailureTest(HomepageCase):
    """首次打开首页、尚未成功读到任何记录时，各类读取失败的表现一致。"""

    def setUp(self):
        # 本用例需要为每种失败形态使用相互独立的浏览器上下文，
        # 默认的 self.context 不使用。
        pass

    def test_all_failure_modes_on_first_load(self):
        for mode in FAILURE_MODES:
            with self.subTest(mode=mode):
                context = _browser.new_context()
                try:
                    page = context.new_page()
                    ListGate(mode=mode).install(page)
                    page.goto(self.base_url + "/")
                    wait_list_failure(page)
                    self.assert_initial_load_failure(list_state(page))
                finally:
                    context.close()


class EmptyListThenFailureTest(HomepageCase):
    """上次成功读取到的就是空列表：再失败时必须改为失败提示。"""

    def test_empty_success_then_failure_then_recover_shows_empty_again(self):
        # 服务端确实没有员工：第一次读取放行，得到有效空列表。
        gate = ListGate(mode="abort", schedule={1: "normal"})
        gate.install(self.page)
        self.open_homepage()

        wait_empty_prompt(self.page)
        state = list_state(self.page)
        self.assertIn("还没有员工记录", state["emptyText"])
        self.assertFalse(state["hasTable"])

        # 第二次读取失败：空结果不再被当成当前确定事实。
        trigger_page_list_reload(self.page)
        wait_list_failure(self.page)
        self.assert_initial_load_failure(list_state(self.page))

        # 再次读取成功且确实为空：恢复空记录提示，失败提示消失。
        gate.mode = "normal"
        trigger_page_list_reload(self.page)
        wait_empty_prompt(self.page)
        state = list_state(self.page)
        self.assertIn("还没有员工记录", state["emptyText"])
        self.assertIsNone(state["errorText"])
        self.assertFalse(state["hasTable"])


class ServedEmptyPayloadThenFailureTest(HomepageCase):
    """成功返回过空列表后失败：即使服务端已有员工，也不得显示未读到的人。"""

    BASELINE = {
        "employee_no": "E300",
        "name": "服务端存在但页面未读到·冯丁",
        "department": "行政部",
        "position": "行政专员",
        "effective_date": "2026-07-01",
        "phone": "13500000300",
        "email": "hidden.e300@example.com",
    }

    def test_empty_payload_last_success_then_abort_shows_failure_only(self):
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline}")

        # 第一次读取被篡改为有效空响应（页面据此显示空态）；
        # 第二次读取直接失败。
        gate = ListGate(schedule={1: "empty", 2: "abort"})
        gate.install(self.page)
        self.open_homepage()

        wait_empty_prompt(self.page)
        self.assertIn("E300", json.dumps(
            list_employees_api(self.base_url), ensure_ascii=False
        ), "基线员工确实已在服务端")

        trigger_page_list_reload(self.page)
        wait_list_failure(self.page)
        state = list_state(self.page)
        # 页面从未成功读到过基线员工：失败后只能显示失败提示，
        # 不能显示空态，也不能“补显示”从未读取成功的基线员工。
        self.assert_initial_load_failure(state)
        self.assertNotIn(self.BASELINE["name"], state["fullText"])
        self.assertNotIn(self.BASELINE["employee_no"], state["fullText"])


class StaleRecordsOnFailureTest(HomepageCase):
    """已成功展示过员工后读取失败：保留旧记录并说明，恢复后按新结果展示。"""

    BASELINE_1 = {
        "employee_no": "E601",
        "name": "上次记录·甲研发",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-05-10",
        "phone": "13600000601",
        "email": "stale.e601@example.com",
    }
    BASELINE_2 = {
        "employee_no": "E602",
        "name": "上次记录·乙市场",
        "department": "市场部",
        "position": "市场专员",
        "effective_date": "2026-06-20",
        "phone": "13600000602",
        "email": "stale.e602@example.com",
    }
    # 列表失败期间服务端新增的员工：失败时绝不能混入，恢复后必须出现。
    NEWCOMER = {
        "employee_no": "E603",
        "name": "失败期间入职·丙新人",
        "department": "产品部",
        "position": "产品经理",
        "effective_date": "2026-10-03",
        "phone": "13600000603",
        "email": "fresh.e603@example.com",
    }

    def test_failure_keeps_last_records_then_recovery_shows_latest(self):
        status, b1 = request(self.base_url, "POST", "/api/employees", self.BASELINE_1)
        self.assertEqual(status, 201, f"基线档案 1 应建档成功: {b1}")
        status, b2 = request(self.base_url, "POST", "/api/employees", self.BASELINE_2)
        self.assertEqual(status, 201, f"基线档案 2 应建档成功: {b2}")

        gate = ListGate(mode="abort")  # 首次读取先放行
        gate.schedule = {1: "normal"}
        gate.install(self.page)
        self.open_homepage()
        self.page.wait_for_selector("#list table", timeout=10000)

        old_rows = [expected_row(b1), expected_row(b2)]
        self.assertEqual(list_state(self.page)["rows"], old_rows,
                         "打开页面后应正常展示两名已有员工")

        # 服务端在页面不知情时新增第三名员工，随后页面读取失败。
        status, b3 = request(self.base_url, "POST", "/api/employees", self.NEWCOMER)
        self.assertEqual(status, 201, f"新员工应在服务端建档成功: {b3}")
        trigger_page_list_reload(self.page)
        wait_list_failure(self.page)

        state = list_state(self.page)
        self.assert_stale_list(state, old_rows)
        # 没读到的新员工不得混入；每条旧记录全字段核对，杜绝重复添加一份。
        self.assertNotIn(b3["employee_no"], state["fullText"])
        self.assertNotIn(b3["name"], state["fullText"])
        self.assertEqual(
            [row[0] for row in state["rows"]],
            [b1["employee_no"], b2["employee_no"]],
        )

        # 服务端确认此刻最新列表其实是三人。
        self.assertEqual(
            [e["employee_no"] for e in list_employees_api(self.base_url)],
            ["E601", "E602", "E603"],
        )

        # 同一页面再次读取成功：按新结果展示三人，失败提示撤除。
        gate.mode = "normal"
        trigger_page_list_reload(self.page)
        self.page.wait_for_function(
            """() => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === 'E603')""",
            timeout=10000,
        )
        state = list_state(self.page)
        self.assertEqual(state["rows"], [expected_row(b1), expected_row(b2), expected_row(b3)],
                         "恢复读取后列表必须与最新接口结果逐行对应")
        self.assertIsNone(state["errorText"], "成功取得最新列表后不得再带失败提示")
        self.assertNotIn("上次读取", state["fullText"])
        self.assertNotIn("加载中", state["fullText"])


class SaveRefreshFailureWithRecordsTest(HomepageCase):
    """保存成功、列表刷新失败：页面上已有非空记录时保留旧记录。"""

    BASELINE = {
        "employee_no": "E701",
        "name": "保存场景基线·吴旧",
        "department": "人事部",
        "position": "HRBP",
        "effective_date": "2026-04-01",
        "phone": "13700000701",
        "email": "base.e701@example.com",
    }
    NEW = {
        "employee_no": " n901 ",
        "name": " 保存成功但刷新失败·郑新 ",
        "department": " 研发部 ",
        "position": " 前端工程师 ",
        "effective_date": "2026-10-04",
        "phone": " 13700000901 ",
        "email": " saved.n901@example.com ",
    }

    def test_save_succeeds_form_clears_stale_list_kept_api_confirms(self):
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline}")

        # 第一次 GET（打开页面）放行；第二次 GET（保存后刷新）失败。
        gate = ListGate(mode="abort", schedule={1: "normal"})
        gate.install(self.page)
        self.open_homepage()
        self.page.wait_for_selector("#list table", timeout=10000)
        self.assertEqual(list_state(self.page)["rows"], [expected_row(baseline)])

        fill_form(self.page, self.NEW)
        submit_form(self.page)
        wait_form_finished(self.page)

        form = form_state(self.page)
        # 明确提示“档案已保存，但员工列表刷新失败”；保存错误提示不出现。
        self.assertFalse(form["note"]["hidden"], "必须提示档案已保存但列表刷新失败")
        self.assertIn("档案已保存", form["note"]["text"])
        self.assertIn("员工列表刷新失败", form["note"]["text"])
        self.assertTrue(form["error"]["hidden"],
                        f"保存错误提示不应出现: {form['error']['text']!r}")
        # 表单按“保存成功”的规则清空，而不是保留输入。
        self.assertEqual(form["values"], {name: "" for name in FORM_FIELDS},
                         "保存成功后即使列表刷新失败，表单也应清空")

        # 列表保留上次成功取得的旧记录，新档案尚未显示不代表保存失败。
        state = list_state(self.page)
        self.assert_stale_list(state, [expected_row(baseline)])
        self.assertNotIn("n901", state["fullText"])

        # 浏览器拦截之外的接口查询：新员工确实已保存，字段按既有规则
        # （去首尾空白、状态在职），原有档案逐字段不变。
        employees = list_employees_api(self.base_url)
        self.assertEqual([e["employee_no"] for e in employees], ["E701", "n901"])
        self.assertEqual(employees[0], baseline, "原有档案内容必须保持不变")
        created = employees[1]
        self.assertEqual(created["name"], "保存成功但刷新失败·郑新")
        self.assertEqual(created["department"], "研发部")
        self.assertEqual(created["position"], "前端工程师")
        self.assertEqual(created["effective_date"], "2026-10-04")
        self.assertEqual(created["phone"], "13700000901")
        self.assertEqual(created["email"], "saved.n901@example.com")
        self.assertEqual(created["status"], "在职")

        # 随后重新取得最新列表（重开首页；此时拦截已恢复放行）：
        # 新结果正常展示，不再带列表失败提示或保存侧提示。
        gate.mode = "normal"
        self.page.reload()
        self.page.wait_for_function(
            """() => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === 'n901')""",
            timeout=10000,
        )
        state = list_state(self.page)
        self.assertEqual(
            state["rows"], [expected_row(baseline), expected_row(created)],
            "重新取得最新列表后应按新结果展示",
        )
        self.assertIsNone(state["errorText"])
        form = form_state(self.page)
        self.assertTrue(form["note"]["hidden"])
        self.assertTrue(form["error"]["hidden"])


class SaveRefreshFailureAfterEmptyListTest(HomepageCase):
    """保存成功、列表刷新失败：此前只成功读到过空列表时显示加载失败。"""

    NEW = {
        "employee_no": " n902 ",
        "name": " 空列表后保存·王新 ",
        "department": " 销售部 ",
        "position": " 销售助理 ",
        "effective_date": "2026-10-04",
        "phone": " 13700000902 ",
        "email": " saved.n902@example.com ",
    }

    def test_save_after_empty_list_refresh_failure_shows_load_error(self):
        # 全新数据目录：首次读取放行，确实得到空员工列表。
        ListGate(mode="badjson", schedule={1: "normal"}).install(self.page)
        self.open_homepage()
        wait_empty_prompt(self.page)

        fill_form(self.page, self.NEW)
        submit_form(self.page)
        wait_form_finished(self.page)

        form = form_state(self.page)
        self.assertFalse(form["note"]["hidden"])
        self.assertIn("档案已保存", form["note"]["text"])
        self.assertIn("员工列表刷新失败", form["note"]["text"])
        self.assertTrue(form["error"]["hidden"],
                        f"保存错误提示不应出现: {form['error']['text']!r}")
        self.assertEqual(form["values"], {name: "" for name in FORM_FIELDS})

        # 上次成功结果是空列表：此时只能显示加载失败，不能回到空态，
        # 也不能出现空表格；新档案未显示不代表保存失败。
        self.assert_initial_load_failure(list_state(self.page))

        employees = list_employees_api(self.base_url)
        self.assertEqual(len(employees), 1)
        created = employees[0]
        self.assertEqual(created["employee_no"], "n902")
        self.assertEqual(created["name"], "空列表后保存·王新")
        self.assertEqual(created["department"], "销售部")
        self.assertEqual(created["position"], "销售助理")
        self.assertEqual(created["effective_date"], "2026-10-04")
        self.assertEqual(created["phone"], "13700000902")
        self.assertEqual(created["email"], "saved.n902@example.com")
        self.assertEqual(created["status"], "在职")


class SaveRefreshFailureWhenNeverLoadedTest(HomepageCase):
    """保存成功、列表刷新失败：从未成功读到过列表时同样显示加载失败。"""

    NEW = {
        "employee_no": " n903 ",
        "name": " 首屏失败后保存·陈新 ",
        "department": " 客服部 ",
        "position": " 客服专员 ",
        "effective_date": "2026-10-04",
        "phone": " 13700000903 ",
        "email": " saved.n903@example.com ",
    }

    def test_save_when_first_load_never_succeeded(self):
        # 打开页面时的首次读取就失败。
        ListGate(mode="status500").install(self.page)
        self.open_homepage()
        wait_list_failure(self.page)
        self.assert_initial_load_failure(list_state(self.page))

        fill_form(self.page, self.NEW)
        submit_form(self.page)
        wait_form_finished(self.page)

        form = form_state(self.page)
        self.assertFalse(form["note"]["hidden"])
        self.assertIn("档案已保存", form["note"]["text"])
        self.assertIn("员工列表刷新失败", form["note"]["text"])
        self.assertTrue(form["error"]["hidden"],
                        f"保存错误提示不应出现: {form['error']['text']!r}")
        self.assertEqual(form["values"], {name: "" for name in FORM_FIELDS})

        self.assert_initial_load_failure(list_state(self.page))

        employees = list_employees_api(self.base_url)
        self.assertEqual(len(employees), 1)
        created = employees[0]
        self.assertEqual(created["employee_no"], "n903")
        self.assertEqual(created["name"], "首屏失败后保存·陈新")
        self.assertEqual(created["department"], "客服部")
        self.assertEqual(created["position"], "客服专员")
        self.assertEqual(created["effective_date"], "2026-10-04")
        self.assertEqual(created["phone"], "13700000903")
        self.assertEqual(created["email"], "saved.n903@example.com")
        self.assertEqual(created["status"], "在职")


if __name__ == "__main__":
    unittest.main(verbosity=2)
