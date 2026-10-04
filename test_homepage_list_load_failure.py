#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""首页员工列表读取失败的端到端界面回归测试。

首页在两个时机会读取员工列表：打开页面时的首次读取，以及成功保存新档案
后的再次读取。读取失败与“没有员工”是两种完全不同的状态，本模块用真实
浏览器（Playwright 驱动本机 Chrome，headless）在真实页面上守住以下区分，
而不是把建档接口返回 201 当作页面表现正确：

首次打开（还没有任何一次成功读取）时读取失败：
1. 加载提示必须结束，列表区域显示明确的中文失败提示（包含“员工列表”
   “加载失败”）；不能显示“还没有员工记录”，也不能留下像正常加载完成
   一样的空表格。三类失败都按同一规则处理：请求未能完成（连接被拒）、
   返回失败状态（500）、返回内容不能解析为带员工数组的有效结果（200 的
   非 JSON 文本、JSON 缺 employees、employees 不是数组）。
2. 只有有效响应确实返回空员工数组时才显示现有空记录提示；上一次成功
   读取的就是空列表时，随后的读取失败必须改为失败提示，不能再把空结果
   当成当前的确定事实，也不能显示空表格。

页面已成功展示过非空列表之后读取失败：
3. 保留上一次成功取得的记录：工号、姓名、部门、岗位、状态、生效日期、
   联系方式逐字段原样；同时用中文说明当前显示的是上次读取的记录、本次
   没有取得最新列表。不能清掉旧记录、重复添加一份，也不能把没有读到的
   新员工混入列表（失败期间经接口建档的员工不得出现）。
4. 之后通过页面已有操作（重新打开页面）成功取得最新列表时，按新结果
   正常展示，新员工出现，且不再带着读取失败的提示。

同一失败发生在成功建档之后：
5. 资料合法且工号未使用、保存已经成功、仅随后读取列表失败时：表单仍按
   成功保存的规则清空，明确提示“档案已保存，但员工列表刷新失败”，保存
   错误提示不出现；列表仍按“是否曾成功取得非空记录”决定保留旧记录还是
   显示加载失败。新档案尚未显示不代表保存失败——实际保存的员工可以从
   员工列表接口查到，原有档案内容保持不变。
6. 对比：保存被拒（工号重复）时即使列表读取同样失败，也必须显示保存
   错误（工号已存在）、表单保留填写内容，不得出现“档案已保存”的刷新
   提示，也不得新增档案。

运行方式（需要 Playwright 与本机 Chrome）：

    .venv/bin/python test_homepage_list_load_failure.py

建档校验、工号唯一性与档案字段展示规则继续由
test_text_field_validation.py、test_effective_date_validation.py、
test_employee_no_uniqueness.py、test_homepage_employee_form.py 与
test_special_text_display.py 覆盖，本模块不改变任何建档规则。
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

# 页面在三种列表状态下的关键中文文案。
LOADING_TEXT = "加载中"
EMPTY_TEXT = "还没有员工记录"
FAIL_TEXT = "加载失败"
STALE_TEXT = "上次读取"
SAVED_BUT_REFRESH_FAILED_TEXT = "档案已保存，但员工列表刷新失败"

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
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-list-fail-test-")
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


def box_state(page, box_id):
    """返回表单提示框的 {hidden, text}。"""
    return page.evaluate(
        """(id) => {
            const box = document.getElementById(id);
            return {hidden: box.hidden, text: box.textContent};
        }""",
        box_id,
    )


def list_state(page):
    """读取列表区域当前完整状态：加载提示是否结束、是否有表格/空态/失败提示。"""
    return page.evaluate(
        """() => {
            const root = document.getElementById('list');
            return {
                text: root.textContent,
                tableCount: root.querySelectorAll('table').length,
                rowCount: root.querySelectorAll('tbody tr').length,
                hasEmpty: !!root.querySelector('.empty'),
                errorCount: root.querySelectorAll('.msg.error, .error').length,
            };
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


def wait_for_load_attempt(page):
    """等待页面打开时的首次列表读取结束（成功或失败都算）。

    页面没有暴露读取状态，这里以“加载中”提示消失为信号：无论成功渲染
    表格/空态还是失败提示，加载提示都会同时结束。
    """
    page.wait_for_function(
        """() => {
            const root = document.getElementById('list');
            return root.textContent.indexOf('加载中') === -1;
        }""",
        timeout=10000,
    )
    # 等一个宏任务，确保失败提示/说明节点已经写入 DOM。
    page.wait_for_timeout(100)


def trigger_list_reload(page):
    """通过页面已有的 load() 再读取一次列表，等待读取与渲染结束。

    页面在打开与成功保存后调用的就是这个函数；页面本身没有“刷新列表”
    按钮，这里直接调用页面自己的加载函数来制造“成功展示过后再次读取”，
    不重新加载整页（整页刷新会清空上次成功记录的内存缓存）。断言只针对
    用户实际看到的 DOM；await load() 返回时成功/失败的渲染已同步完成。
    """
    page.evaluate("async () => { await load(); }")


def submit_form(page):
    page.click("#emp-form button[type=submit]")


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class HomepageListFailureCase(unittest.TestCase):
    """每个用例使用独立的服务进程与数据目录，用例间互不影响。

    各用例都会经接口或表单建档，固定工号在用例间重复，因此不像
    test_homepage_employee_form.py 那样一类共用一个数据目录，而是每个
    用例都从全新数据目录开始。
    """

    def setUp(self):
        self.base_url, self.proc, self.tmpdir = start_server()
        self.context = _browser.new_context()
        self.page = self.context.new_page()

    def tearDown(self):
        self.context.close()
        stop_server(self.proc, self.tmpdir)

    def assert_failure_notice_shown(self, state, message):
        """首次读取失败的标准形态：中文失败提示，无空态，无表格。"""
        self.assertIn(FAIL_TEXT, state["text"], message)
        self.assertIn("员工列表", state["text"], f"{message}：失败提示必须点名员工列表")
        self.assertNotIn(EMPTY_TEXT, state["text"], f"{message}：不能误报为没有员工")
        self.assertNotIn(LOADING_TEXT, state["text"], f"{message}：加载提示必须结束")
        self.assertEqual(state["tableCount"], 0, f"{message}：不能留下空表格")
        self.assertEqual(state["rowCount"], 0, f"{message}：不能留下任何数据行")
        self.assertFalse(state["hasEmpty"], f"{message}：不得显示空记录样式的提示")
        self.assertGreaterEqual(state["errorCount"], 1, f"{message}：应有明确的失败提示元素")

    def assert_save_error_visible(self):
        state = box_state(self.page, "form-error")
        self.assertFalse(state["hidden"], "保存错误提示必须显示")
        return state["text"]

    def assert_save_error_hidden(self):
        state = box_state(self.page, "form-error")
        self.assertTrue(state["hidden"], f"保存错误提示不应显示，当前文本: {state['text']!r}")


class FirstLoadFailureTest(HomepageListFailureCase):
    """首次打开、还未成功读到任何员工记录时，各类读取失败的表现。"""

    def test_network_failure_on_first_load(self):
        """请求未能完成（连接被拒）：失败提示，无空态、无空表格。"""
        # 页面就绪后的首次列表读取即被中止（请求未能完成）。
        self.page.route("**/api/employees", lambda route: route.abort())
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assert_failure_notice_shown(
            list_state(self.page), "首次读取请求未能完成时应显示失败提示"
        )

    def test_failed_status_on_first_load(self):
        """列表请求返回失败状态（500）：失败提示，无空态、无空表格。"""
        def handle(route):
            route.fulfill(status=500, content_type="application/json",
                          body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False))

        self.page.route("**/api/employees", handle)
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assert_failure_notice_shown(
            list_state(self.page), "首次读取返回 500 时应显示失败提示"
        )

    def test_invalid_payloads_on_first_load(self):
        """200 但内容不能解析为带员工数组的有效结果：均按读取失败处理。"""
        invalid_bodies = [
            ("not-json-text", "非 JSON 文本"),
            (json.dumps({"data": []}), "JSON 但缺少 employees"),
            (json.dumps({"employees": {"id": 1}}), "employees 不是数组"),
            (json.dumps(["employees"]), "顶层是数组而非含 employees 的对象"),
        ]
        for body, description in invalid_bodies:
            with self.subTest(description=description):
                context = _browser.new_context()
                page = context.new_page()

                def make_handler(payload):
                    def handle(route):
                        route.fulfill(status=200, content_type="application/json; charset=utf-8",
                                      body=payload)
                    return handle

                page.route("**/api/employees", make_handler(body))
                page.goto(self.base_url + "/")
                wait_for_load_attempt(page)
                try:
                    self.assert_failure_notice_shown(
                        list_state(page),
                        f"首次读取返回{description}时应按失败处理",
                    )
                finally:
                    context.close()

    def test_valid_empty_list_shows_empty_hint(self):
        """对照：有效响应确实返回空员工数组时，才显示现有空记录提示。"""
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        state = list_state(self.page)
        self.assertIn(EMPTY_TEXT, state["text"], "有效空列表应显示空记录提示")
        self.assertNotIn(FAIL_TEXT, state["text"], "有效空列表不是加载失败")
        self.assertNotIn(LOADING_TEXT, state["text"], "加载提示必须结束")
        self.assertEqual(state["tableCount"], 0, "空列表不应渲染表格")
        self.assertEqual(state["rowCount"], 0)


class EmptyThenFailureTest(HomepageListFailureCase):
    """上一次成功读取的就是空列表，后续读取失败必须改为失败提示。"""

    def test_empty_success_then_failure_shows_failure_notice(self):
        # 全新数据目录：首次读取成功且确实为空。
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        state = list_state(self.page)
        self.assertIn(EMPTY_TEXT, state["text"])
        self.assertNotIn(FAIL_TEXT, state["text"])

        # 随后令页面再次读取列表（页面自己的 load，保留“上次读到空列表”
        # 这一事实），这次返回 500：必须改为失败提示。
        self.page.route(
            "**/api/employees",
            lambda route: route.fulfill(
                status=500,
                content_type="application/json",
                body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False),
            ),
        )
        trigger_list_reload(self.page)
        self.assert_failure_notice_shown(
            list_state(self.page),
            "上次成功读取为空列表后再次读取失败，应改为失败提示，不能继续把空结果当事实",
        )

    def test_empty_success_then_network_failure_shows_failure_notice(self):
        """空列表读取成功后，下一次请求根本未能完成，同样不能再显示空态。"""
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assertIn(EMPTY_TEXT, list_state(self.page)["text"])

        self.page.route("**/api/employees", lambda route: route.abort())
        trigger_list_reload(self.page)
        self.assert_failure_notice_shown(
            list_state(self.page),
            "空列表成功后请求未能完成，应显示失败提示而非空记录提示",
        )


class StaleRecordsOnFailureTest(HomepageListFailureCase):
    """已成功展示过非空列表，之后读取失败：保留上次记录并附中文说明。"""

    BASELINE = {
        "employee_no": "F301",
        "name": "列表失败基线·周甲",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-09-01",
        "phone": "13700000001",
        "email": "stale.f301@example.com",
    }

    # 失败期间经接口建档的员工：页面保留旧列表时绝不允许混入。
    UNSEEN = {
        "employee_no": "F302",
        "name": "失败期间建档·吴乙",
        "department": "市场部",
        "position": "市场专员",
        "effective_date": "2026-09-20",
        "phone": "13700000002",
        "email": "unseen.f302@example.com",
    }

    def _open_with_baseline(self):
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline}")
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        state = list_state(self.page)
        self.assertNotIn(FAIL_TEXT, state["text"], "前置读取应成功")
        self.assertEqual(list_rows(self.page), [expected_row(baseline)])
        return baseline

    def _fail_next_list_load(self):
        """让随后的 /api/employees 读取恰好失败一次，之后恢复真实服务。"""
        state = {"failed": False}

        def handle(route):
            if not state["failed"]:
                state["failed"] = True
                route.fulfill(
                    status=500,
                    content_type="application/json",
                    body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False),
                )
            else:
                route.continue_()

        self.page.route("**/api/employees", handle)

    def test_failure_after_nonempty_success_keeps_stale_records(self):
        baseline = self._open_with_baseline()

        # 失败期间，服务端其实多了一名新员工：页面本次读不到，绝不能混入。
        status, unseen = request(self.base_url, "POST", "/api/employees", self.UNSEEN)
        self.assertEqual(status, 201, f"失败期间的档案仍应真实保存: {unseen}")

        self._fail_next_list_load()
        trigger_list_reload(self.page)

        state = list_state(self.page)
        # 中文说明：当前显示的是上次读取的记录，本次没有取得最新列表。
        self.assertIn(FAIL_TEXT, state["text"], "读取失败应有失败提示")
        self.assertIn(STALE_TEXT, state["text"], "必须说明显示的是上次读取的记录")
        self.assertIn("本次", state["text"], "必须说明本次未能取得最新列表")
        self.assertNotIn(EMPTY_TEXT, state["text"], "有旧记录时不能误报为空")

        # 旧记录逐字段原样保留，且只有一份（不重复添加）。
        rows = list_rows(self.page)
        self.assertEqual(
            rows,
            [expected_row(baseline)],
            "读取失败时必须保留上一次成功取得的记录且不能重复一份",
        )
        self.assertEqual(len(rows), 1)
        self.assertEqual(
            [cell for row in rows for cell in row].count(self.UNSEEN["name"]),
            0,
            "没有读到的新员工不得混入列表",
        )
        self.assertNotIn(self.UNSEEN["employee_no"], state["text"])

    def test_recovered_load_replaces_stale_records_and_clears_notice(self):
        """之后成功取得最新列表：按新结果展示，不再带失败提示。"""
        self._open_with_baseline()

        # 制造一次失败（保留旧记录）。
        self._fail_next_list_load()
        trigger_list_reload(self.page)
        self.assertIn(STALE_TEXT, list_state(self.page)["text"])

        # 此时服务端已有新员工（失败期间建档）。
        status, unseen = request(self.base_url, "POST", "/api/employees", self.UNSEEN)
        self.assertEqual(status, 201)

        # 打开新页面（页面已有操作：重新打开首页）成功取得最新列表。
        context = _browser.new_context()
        self.addCleanup(context.close)
        fresh_page = context.new_page()
        fresh_page.goto(self.base_url + "/")
        wait_for_load_attempt(fresh_page)
        state = list_state(fresh_page)
        self.assertNotIn(FAIL_TEXT, state["text"], "成功取得列表后不应再带失败提示")
        self.assertNotIn(STALE_TEXT, state["text"])
        self.assertNotIn(LOADING_TEXT, state["text"])

        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["employee_no"] for row in employees],
            [self.BASELINE["employee_no"], self.UNSEEN["employee_no"]],
        )
        self.assertEqual(
            list_rows(fresh_page),
            [expected_row(row) for row in employees],
            "恢复读取后列表应按新结果正常展示（含此前没读到的新员工）",
        )

    def test_stale_view_then_successful_reload_in_same_page(self):
        """同一页面里失败后再成功读取一次：旧提示消失，内容更新为最新结果。"""
        self._open_with_baseline()

        # 先失败一次（页面自己的 load，保留上次成功记录缓存）。
        self._fail_next_list_load()
        trigger_list_reload(self.page)
        self.assertIn(STALE_TEXT, list_state(self.page)["text"])

        # 解除拦截，再在同一页面通过页面自己的 load() 成功读取一次。
        self.page.unroute("**/api/employees")
        status, unseen = request(self.base_url, "POST", "/api/employees", self.UNSEEN)
        self.assertEqual(status, 201)
        trigger_list_reload(self.page)

        state = list_state(self.page)
        self.assertNotIn(FAIL_TEXT, state["text"], "再次读取成功后失败提示必须消失")
        self.assertNotIn(STALE_TEXT, state["text"], "不得继续声称显示的是上次记录")
        employees = list_employees(self.base_url)
        self.assertEqual(
            list_rows(self.page),
            [expected_row(row) for row in employees],
            "成功读取后应以最新结果替换暂存记录",
        )


class SaveSucceedsButRefreshFailsTest(HomepageListFailureCase):
    """保存成功、仅随后列表刷新失败：表单清空 + 保存成功但刷新失败提示。"""

    BASELINE = {
        "employee_no": "S401",
        "name": "刷新失败基线·冯丁",
        "department": "人事部",
        "position": "HRBP",
        "effective_date": "2026-08-20",
        "phone": "13600000004",
        "email": "baseline.s401@example.com",
    }

    NEW = {
        "employee_no": " S402 ",
        "name": " 保存成功刷新失败·陈戊 ",
        "department": " 研发部 ",
        "position": " 测试工程师 ",
        "effective_date": "2026-10-02",
        "phone": " 13800000005 ",
        "email": " saved.refreshfail.s402@example.com ",
    }
    NEW_NO = "S402"

    def _wait_refresh_failure_note(self):
        self.page.wait_for_selector("#form-note:not([hidden])", timeout=10000)

    def test_saved_but_refresh_fails_with_baseline_keeps_stale_list(self):
        """曾成功取得非空记录：表单清空、提示已保存但刷新失败、保留旧记录。"""
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline}")

        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assertEqual(list_rows(self.page), [expected_row(baseline)])

        # 保存请求必须真正到达服务，只有保存之后的列表读取失败：
        # POST 放行，GET /api/employees 返回 500。
        def handle(route):
            if route.request.method == "GET":
                route.fulfill(
                    status=500,
                    content_type="application/json",
                    body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False),
                )
            else:
                route.continue_()

        self.page.route("**/api/employees", handle)

        fill_form(self.page, self.NEW)
        submit_form(self.page)
        self._wait_refresh_failure_note()

        # 明确提示“档案已保存，但员工列表刷新失败”。
        note = box_state(self.page, "form-note")
        self.assertFalse(note["hidden"], "保存成功但刷新失败的提示必须显示")
        self.assertIn(SAVED_BUT_REFRESH_FAILED_TEXT, note["text"])

        # 保存错误提示不得出现（保存本身确实成功了）。
        self.assert_save_error_hidden()

        # 表单仍按成功保存的规则清空。
        self.assertEqual(
            read_form(self.page),
            {name: "" for name in FORM_FIELDS},
            "保存成功后即使列表刷新失败，表单也应清空",
        )

        # 列表保留上一次成功取得的基线记录，新档案暂未显示，并附中文说明。
        state = list_state(self.page)
        self.assertIn(STALE_TEXT, state["text"], "应说明当前显示的是上次读取的记录")
        self.assertIn(FAIL_TEXT, state["text"])
        self.assertEqual(
            list_rows(self.page),
            [expected_row(baseline)],
            "新档案尚未显示时应保留旧记录，不能清空或重复",
        )
        self.assertNotIn(self.NEW_NO, state["text"])

        # 新档案尚未显示不代表保存失败：接口里可以查到实际保存的员工，
        # 原有档案内容保持不变。
        self.page.unroute("**/api/employees")
        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["employee_no"] for row in employees],
            [self.BASELINE["employee_no"], self.NEW_NO],
            "保存的员工必须能从员工列表接口查到",
        )
        self.assertEqual(employees[0], baseline, "原有档案内容必须保持不变")
        created = employees[1]
        self.assertEqual(created["name"], "保存成功刷新失败·陈戊")
        self.assertEqual(created["department"], "研发部")
        self.assertEqual(created["position"], "测试工程师")
        self.assertEqual(created["effective_date"], self.NEW["effective_date"])
        self.assertEqual(created["phone"], "13800000005")
        self.assertEqual(created["email"], "saved.refreshfail.s402@example.com")
        self.assertEqual(created["status"], "在职")

    def test_saved_but_refresh_fails_without_prior_records(self):
        """此前只成功读取过空列表：提示已保存但刷新失败，列表显示加载失败。"""
        # 全新数据目录，先让首页成功读取一次空列表。
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assertIn(EMPTY_TEXT, list_state(self.page)["text"])

        def handle(route):
            if route.request.method == "GET":
                route.fulfill(
                    status=500,
                    content_type="application/json",
                    body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False),
                )
            else:
                route.continue_()

        self.page.route("**/api/employees", handle)

        fill_form(self.page, self.NEW)
        submit_form(self.page)
        self._wait_refresh_failure_note()

        note = box_state(self.page, "form-note")
        self.assertIn(SAVED_BUT_REFRESH_FAILED_TEXT, note["text"])
        self.assert_save_error_hidden()
        self.assertEqual(
            read_form(self.page),
            {name: "" for name in FORM_FIELDS},
            "保存成功后表单应清空，即使列表从未取得过非空记录",
        )

        state = list_state(self.page)
        self.assertIn(FAIL_TEXT, state["text"], "没有旧记录时列表应显示加载失败")
        self.assertIn("员工列表", state["text"], "失败提示必须点名员工列表")
        self.assertNotIn(EMPTY_TEXT, state["text"], "不能误报为没有员工")
        self.assertNotIn(LOADING_TEXT, state["text"], "加载提示必须结束")
        self.assertNotIn(STALE_TEXT, state["text"], "没有上次记录可显示，不应出现暂存说明")
        self.assertEqual(state["tableCount"], 0, "不能留下空表格")
        self.assertEqual(state["rowCount"], 0)
        self.assertNotIn(self.NEW_NO, state["text"], "新档案尚未显示不代表保存失败")

        # 接口侧：新档案确已保存。
        self.page.unroute("**/api/employees")
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), 1)
        self.assertEqual(employees[0]["employee_no"], self.NEW_NO)
        self.assertEqual(employees[0]["status"], "在职")

    def test_saved_but_refresh_request_aborted(self):
        """保存成功但刷新请求未能完成（连接中止）：与失败状态同等对待。"""
        status, baseline = request(self.base_url, "POST", "/api/employees", self.BASELINE)
        self.assertEqual(status, 201)
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)

        def handle(route):
            if route.request.method == "GET":
                route.abort()
            else:
                route.continue_()

        self.page.route("**/api/employees", handle)

        new_payload = dict(self.NEW, employee_no="S403",
                           email="aborted.s403@example.com")
        fill_form(self.page, new_payload)
        submit_form(self.page)
        self._wait_refresh_failure_note()

        self.assertIn(SAVED_BUT_REFRESH_FAILED_TEXT,
                      box_state(self.page, "form-note")["text"])
        self.assert_save_error_hidden()
        self.assertEqual(read_form(self.page),
                         {name: "" for name in FORM_FIELDS})
        self.assertEqual(list_rows(self.page), [expected_row(baseline)],
                         "请求中止时仍保留上一次成功取得的记录")
        self.page.unroute("**/api/employees")
        employees = list_employees(self.base_url)
        self.assertIn("S403", [row["employee_no"] for row in employees])
        self.assertEqual(
            next(row for row in employees if row["employee_no"] == "S401"),
            baseline,
            "原有档案保持不变",
        )


class SaveRejectedWhileListFailingTest(HomepageListFailureCase):
    """对照：保存被拒（工号重复）时，即使列表同样读不到，也不能误报保存成功。"""

    ORIGINAL = {
        "employee_no": "A007",
        "name": "原始档案·赵甲",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-09-01",
        "phone": "13800000001",
        "email": "original.a007@example.com",
    }

    DUPLICATE = {
        "employee_no": " a007 ",
        "name": "重复提交·钱乙",
        "department": "市场部",
        "position": "市场专员",
        "effective_date": "2026-10-02",
        "phone": " 13900000002 ",
        "email": " again.a007@example.com ",
    }

    def test_duplicate_save_rejected_shows_save_error_even_if_list_fails(self):
        status, original = request(self.base_url, "POST", "/api/employees", self.ORIGINAL)
        self.assertEqual(status, 201, f"原档案应建档成功: {original}")
        self.page.goto(self.base_url + "/")
        wait_for_load_attempt(self.page)
        self.assertEqual(list_rows(self.page), [expected_row(original)])

        # POST 继续交给真实服务（返回 409），GET 一律制造失败。即便有缺陷
        # 的实现在保存被拒后也去读列表，这里读到的仍是失败，从而保证用例
        # 区分“保存被拒”与“保存成功但刷新失败”只取决于 POST 的结果。
        def handle(route):
            if route.request.method == "GET":
                route.fulfill(
                    status=500,
                    content_type="application/json",
                    body=json.dumps({"error": "服务器内部错误"}, ensure_ascii=False),
                )
            else:
                route.continue_()

        self.page.route("**/api/employees", handle)
        fill_form(self.page, self.DUPLICATE)
        submit_form(self.page)
        self.page.wait_for_selector("#form-error:not([hidden])", timeout=10000)

        # 必须显示保存错误（工号已存在），而不是“档案已保存但刷新失败”。
        text = self.assert_save_error_visible()
        self.assertIn("工号", text)
        self.assertIn("已存在", text)
        note = box_state(self.page, "form-note")
        self.assertTrue(note["hidden"],
                        f"保存未成功时不得出现“档案已保存”提示: {note['text']!r}")

        # 表单保留刚填写的内容（与现有失败规则一致）。
        self.assertEqual(
            read_form(self.page),
            dict(self.DUPLICATE),
            "保存被拒后填写内容必须原样保留",
        )

        # 接口侧不新增档案，原档案保持不变。
        self.page.unroute("**/api/employees")
        employees = list_employees(self.base_url)
        self.assertEqual(employees, [original], "重复工号不得新增档案")


if __name__ == "__main__":
    unittest.main(verbosity=2)
