#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""首页“新建员工档案”的页面级端到端自动化回归测试。

与接口级测试（工号唯一性、生效日期）互补：本模块通过真实无头 Chrome
加载首页，模拟用户填写表单并点击“保存档案”，断言提交后页面本身的
行为——错误提示、输入保留、表单清空、员工列表自动刷新，而不是把
接口返回成功当作页面已经正确展示的依据。

仅使用 Python 标准库：通过 Chrome DevTools Protocol（WebSocket）驱动
`google-chrome --headless=new`。若环境中没有可用的 Chrome，整个模块
按 SkipTest 跳过，不影响接口级回归。

覆盖场景（沿用现有首页与建档规则，不引入新的员工管理操作）：
1. 合法档案提交成功后，员工列表无需手动刷新整页即出现新员工（工号与
   必填文字为去掉首尾空白后的保存值，状态“在职”，电话与邮箱显示在
   联系方式中），表单七个输入框全部恢复为空，错误提示不显示，且能与
   此前已存在的记录区分开；
2. 已有工号 A007 时，用“ a007 ”再次提交内容明显不同的完整档案：页面
   显示说明工号已存在的中文提示，刚填写的全部内容（含首尾空白与选填
   联系方式）保留在表单中，页面列表与接口查询中的原档案均保持原样，
   失败提交的内容不出现在任何记录里；
3. 只把失败表单中的工号改为未使用的工号再次保存：正常新增为另一名
   员工，原档案保持原样，错误提示消失，表单清空；最终页面列表与
   员工列表接口查询结果逐行对应，既不显示未保存的员工，也不停留在
   错误状态。
"""
import base64
import json
import os
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

APP_PATH = Path(__file__).resolve().parent / "app.py"

CHROME_CANDIDATES = [
    shutil.which("google-chrome"),
    shutil.which("chromium"),
    shutil.which("chromium-browser"),
    "/usr/bin/google-chrome",
]

# ---------------------------------------------------------------------------
# 测试数据：既有记录用于区分“刚创建的员工”；重复提交使用与原档案明显
# 不同的姓名/部门/联系方式，一旦发生覆盖或意外新增，断言能直接暴露。
# ---------------------------------------------------------------------------
BASELINE_EMPLOYEE = {
    "employee_no": "B700",
    "name": "既有员工·秦基",
    "department": "财务部",
    "position": "会计",
    "effective_date": "2026-07-01",
    "phone": "13700000000",
    "email": "baseline.b700@example.com",
}

ORIGINAL_A007 = {
    "employee_no": "A007",
    "name": "原档案·周原",
    "department": "研发部",
    "position": "高级工程师",
    "effective_date": "2026-08-01",
    "phone": "13700000001",
    "email": "original.a007@example.com",
}

# 首页首次合法提交：工号与必填文字刻意带首尾空白，列表应展示去空白后的保存值。
NEW_EMPLOYEE_FORM = {
    "employee_no": "  E701  ",
    "name": " 新档·林新 ",
    "department": " 产品部 ",
    "position": " 产品经理 ",
    "effective_date": "2026-10-05",
    "phone": " 13600000002 ",
    "email": " new.e701@example.com ",
}
NEW_EMPLOYEE_SAVED = {key: value.strip() for key, value in NEW_EMPLOYEE_FORM.items()}

# 与原 A007 档案明显不同的重复提交内容。
DUPLICATE_FORM = {
    "employee_no": " a007 ",
    "name": "撞号·吴撞",
    "department": "市场部",
    "position": "市场专员",
    "effective_date": "2026-10-06",
    "phone": "13600000003",
    "email": "dup.a007@example.com",
}

# 失败后改用的未使用工号。
RETRY_EMPLOYEE_NO = "E703"

FORM_FIELD_NAMES = (
    "employee_no", "name", "department", "position",
    "effective_date", "phone", "email",
)

BASE_URL = None
_server_proc = None
_server_tmpdir = None
_chrome_proc = None
_chrome_profile = None
_cdp = None


# ---------------------------------------------------------------------------
# 服务进程管理（与接口级测试相同的启动方式：随机端口 + 临时数据目录）。
# ---------------------------------------------------------------------------
def _start_server():
    global BASE_URL, _server_proc, _server_tmpdir
    _server_tmpdir = tempfile.mkdtemp(prefix="peopledesk-page-test-")
    _server_proc = subprocess.Popen(
        [
            sys.executable,
            str(APP_PATH),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--data-dir",
            _server_tmpdir,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    deadline = time.time() + 10
    line = ""
    while time.time() < deadline:
        line = _server_proc.stdout.readline()
        if not line:
            if _server_proc.poll() is not None:
                raise RuntimeError("服务进程提前退出，未能启动")
            time.sleep(0.05)
            continue
        match = re.search(r"http://[^/\s]+:(\d+)", line)
        if match:
            BASE_URL = f"http://127.0.0.1:{match.group(1)}"
            return
    _stop_server()
    raise RuntimeError(f"未能从服务输出解析监听地址: {line!r}")


def _stop_server():
    global _server_proc, _server_tmpdir
    if _server_proc is not None and _server_proc.poll() is None:
        _server_proc.send_signal(signal.SIGTERM)
        try:
            _server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _server_proc.kill()
            _server_proc.wait(timeout=5)
        if _server_proc.stdout is not None:
            _server_proc.stdout.close()
    _server_proc = None
    if _server_tmpdir is not None:
        shutil.rmtree(_server_tmpdir, ignore_errors=True)
        _server_tmpdir = None


# ---------------------------------------------------------------------------
# 极简 WebSocket 客户端（标准库实现，足以与 Chrome DevTools 通信）。
# ---------------------------------------------------------------------------
class _WebSocket:
    def __init__(self, url, timeout=15):
        match = re.match(r"ws://([^:/]+):(\d+)(/.*)$", url)
        if not match:
            raise ValueError(f"无法解析的 WebSocket 地址: {url!r}")
        host, port, path = match.group(1), int(match.group(2)), match.group(3)
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(handshake.encode("ascii"))
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("WebSocket 握手被中断")
            response += chunk
        status_line = response.split(b"\r\n", 1)[0]
        if b" 101" not in status_line:
            raise ConnectionError(f"WebSocket 握手失败: {status_line!r}")

    def _read_exact(self, count):
        buf = b""
        while len(buf) < count:
            chunk = self.sock.recv(count - len(buf))
            if not chunk:
                raise ConnectionError("WebSocket 连接被关闭")
            buf += chunk
        return buf

    def _send_frame(self, opcode, data):
        header = bytes([0x80 | opcode])
        length = len(data)
        if length < 126:
            header += bytes([0x80 | length])
        elif length < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", length)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", length)
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(data))
        self.sock.sendall(header + mask + masked)

    def send_text(self, text):
        self._send_frame(0x1, text.encode("utf-8"))

    def recv_message(self):
        payload = b""
        while True:
            first, second = self._read_exact(2)
            fin = first & 0x80
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read_exact(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read_exact(8))[0]
            mask = self._read_exact(4) if second & 0x80 else None
            data = self._read_exact(length) if length else b""
            if mask:
                data = bytes(byte ^ mask[i % 4] for i, byte in enumerate(data))
            if opcode == 0x9:  # ping → pong
                self._send_frame(0xA, data)
                continue
            if opcode == 0x8:
                raise ConnectionError("WebSocket 收到关闭帧")
            payload += data
            if fin:
                return payload.decode("utf-8")

    def close(self):
        try:
            self._send_frame(0x8, b"")
        except OSError:
            pass
        self.sock.close()


class _CDPClient:
    """与单个页面目标通信的极简 Chrome DevTools Protocol 客户端。"""

    def __init__(self, ws_url):
        self.ws = _WebSocket(ws_url)
        self._next_id = 0

    def call(self, method, params=None):
        self._next_id += 1
        message_id = self._next_id
        self.ws.send_text(json.dumps(
            {"id": message_id, "method": method, "params": params or {}}
        ))
        while True:
            message = json.loads(self.ws.recv_message())
            if message.get("id") != message_id:
                continue  # 忽略事件通知
            if "error" in message:
                raise RuntimeError(f"CDP {method} 失败: {message['error']}")
            return message.get("result", {})

    def evaluate(self, expression):
        """在页面中执行 JS 并返回 JSON 化的结果；页面抛错时抛出异常。"""
        result = self.call("Runtime.evaluate", {
            "expression": expression,
            "returnByValue": True,
            "awaitPromise": True,
        })
        if "exceptionDetails" in result:
            raise RuntimeError(f"页面脚本执行失败: {result['exceptionDetails']}")
        return result.get("result", {}).get("value")

    def close(self):
        self.ws.close()


# ---------------------------------------------------------------------------
# Chrome 进程管理。
# ---------------------------------------------------------------------------
def _find_chrome():
    for candidate in CHROME_CANDIDATES:
        if candidate and os.path.exists(candidate):
            return candidate
    return None


def _start_chrome():
    global _chrome_proc, _chrome_profile, _cdp
    binary = _find_chrome()
    if binary is None:
        raise unittest.SkipTest("未找到可用的 Chrome/Chromium，跳过页面级回归测试")
    _chrome_profile = tempfile.mkdtemp(prefix="peopledesk-chrome-")
    _chrome_proc = subprocess.Popen(
        [
            binary,
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--remote-allow-origins=*",
            "--remote-debugging-port=0",
            f"--user-data-dir={_chrome_profile}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    devtools_url = None
    deadline = time.time() + 20
    while time.time() < deadline:
        line = _chrome_proc.stderr.readline()
        if not line:
            if _chrome_proc.poll() is not None:
                break
            time.sleep(0.05)
            continue
        match = re.search(r"DevTools listening on (ws://\S+)", line)
        if match:
            devtools_url = match.group(1)
            break
    if devtools_url is None:
        _stop_chrome()
        raise unittest.SkipTest("Chrome 未能提供 DevTools 地址，跳过页面级回归测试")

    http_base = re.match(r"ws://(127\.0\.0\.1:\d+)", devtools_url).group(1)
    page_ws_url = None
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(
                f"http://{http_base}/json/list", timeout=5
            ) as resp:
                targets = json.loads(resp.read().decode("utf-8"))
            for target in targets:
                if target.get("type") == "page":
                    page_ws_url = target["webSocketDebuggerUrl"]
                    break
        except (OSError, ValueError):
            pass
        if page_ws_url:
            break
        time.sleep(0.2)
    if page_ws_url is None:
        _stop_chrome()
        raise RuntimeError("未能从 Chrome 获取页面目标")

    _cdp = _CDPClient(page_ws_url)
    _cdp.call("Page.enable")
    _cdp.call("Page.navigate", {"url": BASE_URL + "/"})
    _wait_for_page(
        "document.readyState==='complete' && !!document.getElementById('emp-form')",
        "首页加载完成",
    )


def _stop_chrome():
    global _chrome_proc, _chrome_profile, _cdp
    if _cdp is not None:
        try:
            _cdp.close()
        except Exception:
            pass
        _cdp = None
    if _chrome_proc is not None and _chrome_proc.poll() is None:
        _chrome_proc.send_signal(signal.SIGTERM)
        try:
            _chrome_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _chrome_proc.kill()
            _chrome_proc.wait(timeout=5)
        if _chrome_proc.stderr is not None:
            _chrome_proc.stderr.close()
    _chrome_proc = None
    if _chrome_profile is not None:
        shutil.rmtree(_chrome_profile, ignore_errors=True)
        _chrome_profile = None


def setUpModule():
    _start_server()
    try:
        _start_chrome()
    except Exception:
        _stop_server()
        raise


def tearDownModule():
    _stop_chrome()
    _stop_server()


# ---------------------------------------------------------------------------
# HTTP 接口辅助（用于布置既有数据、核对页面与接口查询结果是否对应）。
# ---------------------------------------------------------------------------
def request(method, path, body=None):
    """发起 HTTP 请求，返回 (status_code, json_body)。"""
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body, ensure_ascii=False).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        BASE_URL + path, data=data, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            payload = json.loads(resp.read().decode("utf-8"))
            return resp.status, payload
    except urllib.error.HTTPError as err:
        payload = json.loads(err.read().decode("utf-8"))
        return err.code, payload


def api_employees():
    status, body = request("GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    return body["employees"]


# ---------------------------------------------------------------------------
# 页面操作与读取辅助：全部通过真实页面 DOM 完成，模拟用户操作。
# ---------------------------------------------------------------------------
def _evaluate(expression):
    return _cdp.evaluate(expression)


def _wait_for_page(condition_js, description, timeout=8):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if _evaluate(f"!!({condition_js})"):
                return
        except RuntimeError:
            pass
        time.sleep(0.1)
    raise AssertionError(f"等待页面状态超时: {description}")


def fill_form(values):
    """像用户一样把内容填进表单各输入框。"""
    payload = json.dumps(values, ensure_ascii=False)
    ok = _evaluate(
        "(function(values){var form=document.getElementById('emp-form');"
        "Object.keys(values).forEach(function(k){form.elements[k].value=values[k];});"
        "return true;})(" + payload + ")"
    )
    assert ok, "表单填写失败"


def click_save():
    """点击“保存档案”按钮，走页面真实的提交逻辑。"""
    _evaluate(
        "document.querySelector('#emp-form button[type=\"submit\"]').click();true"
    )


def read_form_state():
    """读取表单七个输入框的当前值与错误提示的展示状态。"""
    return _evaluate(
        "(function(){var f=document.getElementById('emp-form');"
        "var e=document.getElementById('form-error');"
        "return {employee_no:f.employee_no.value,name:f.name.value,"
        "department:f.department.value,position:f.position.value,"
        "effective_date:f.effective_date.value,phone:f.phone.value,"
        "email:f.email.value,error_hidden:e.hidden,error_text:e.textContent};})()"
    )


def read_list_rows():
    """读取员工列表表格，每行返回 [工号, 姓名, 部门, 岗位, 状态, 生效日期, 联系方式]。"""
    return _evaluate(
        "(function(){var rows=document.querySelectorAll('#list table tbody tr');"
        "return Array.prototype.map.call(rows,function(tr){"
        "return Array.prototype.map.call(tr.cells,function(td){return td.textContent;});"
        "});})()"
    )


def expected_row(record):
    """按页面展示规则，由一条已保存记录推出列表中应显示的行。"""
    contact = " · ".join(
        part for part in (record.get("phone") or "", record.get("email") or "") if part
    ) or "未填写"
    return [
        record["employee_no"],
        record["name"],
        record["department"],
        record["position"],
        record["status"],
        record["effective_date"],
        contact,
    ]


def wait_for_list_row(employee_no, description):
    """等待列表中出现指定工号的行（不刷新整页，仅轮询页面 DOM）。"""
    _wait_for_page(
        "Array.prototype.some.call("
        "document.querySelectorAll('#list table tbody tr'),function(tr){"
        f"return tr.cells.length && tr.cells[0].textContent === {json.dumps(employee_no)};"
        "})",
        description,
    )


def wait_for_error_visible():
    _wait_for_page(
        "!document.getElementById('form-error').hidden",
        "错误提示显示",
    )


def submit_form_and_wait_error(values):
    """填写并提交一份会被拒绝的档案，等待错误提示出现。"""
    fill_form(values)
    click_save()
    wait_for_error_visible()


class HomepageEmployeeFormTest(unittest.TestCase):
    """围绕首页真实页面操作的建档回归：成功、工号冲突、改号后重试。"""

    @classmethod
    def setUpClass(cls):
        if _cdp is None:
            raise unittest.SkipTest("Chrome 不可用")
        # 通过接口布置两条既有档案：一条普通既有员工、一条工号 A007 的原档案，
        # 用于与页面新创建的记录区分，并作为判重冲突的对象。
        status, created = request("POST", "/api/employees", BASELINE_EMPLOYEE)
        assert status == 201, f"既有员工布置失败: {created}"
        cls.baseline_record = created
        status, created = request("POST", "/api/employees", ORIGINAL_A007)
        assert status == 201, f"A007 原档案布置失败: {created}"
        cls.original_a007 = created
        # 让首页列表加载出既有数据。
        _cdp.call("Page.navigate", {"url": BASE_URL + "/"})
        _wait_for_page(
            "document.readyState==='complete' && !!document.getElementById('emp-form')",
            "首页重新加载完成",
        )
        wait_for_list_row("A007", "既有档案出现在首页列表")

    def assert_page_matches_api(self):
        """页面列表与员工列表接口查询结果逐行对应：不多显示、不少显示。"""
        records = api_employees()
        rows = read_list_rows()
        self.assertEqual(
            rows,
            [expected_row(record) for record in records],
            "首页员工列表应与接口查询结果逐行一致",
        )
        return records

    def test_01_valid_submission_updates_list_and_clears_form(self):
        """合法提交：列表无需手动刷新即出现新员工，表单清空，无错误提示。"""
        rows_before = read_list_rows()
        self.assertEqual(len(rows_before), 2, "提交前应只有两条既有档案")
        baseline_row = expected_row(self.baseline_record)
        self.assertIn(baseline_row, rows_before)

        # 在页面上留一个标记：若浏览器整页刷新，该标记会丢失。
        _evaluate("window.__pageMarker='not-reloaded';true")

        fill_form(NEW_EMPLOYEE_FORM)
        click_save()

        # 不刷新整页，等待列表自动出现新工号（去掉首尾空白后的保存值）。
        wait_for_list_row("E701", "新员工出现在员工列表")
        self.assertEqual(
            _evaluate("window.__pageMarker||''"),
            "not-reloaded",
            "列表更新不应依赖整页刷新",
        )

        # 表单七个输入框全部恢复为空，错误提示不显示。
        form = read_form_state()
        for field in FORM_FIELD_NAMES:
            self.assertEqual(form[field], "", f"保存成功后 {field} 应恢复为空")
        self.assertTrue(form["error_hidden"], "合法提交不应显示错误提示")

        # 列表新增一行且既有行原样保留，新行内容按保存值展示。
        rows_after = read_list_rows()
        self.assertEqual(len(rows_after), 3, "列表应只新增一名员工")
        self.assertIn(baseline_row, rows_after, "既有员工记录应保持原样")
        self.assertIn(expected_row(self.original_a007), rows_after)
        new_row = [
            "E701",
            NEW_EMPLOYEE_SAVED["name"],
            NEW_EMPLOYEE_SAVED["department"],
            NEW_EMPLOYEE_SAVED["position"],
            "在职",
            NEW_EMPLOYEE_FORM["effective_date"],
            f"{NEW_EMPLOYEE_SAVED['phone']} · {NEW_EMPLOYEE_SAVED['email']}",
        ]
        self.assertIn(new_row, rows_after, "新员工应按去空白后的保存值展示")

        # 接口查询同样能查到这条新记录，且页面与接口一致。
        records = self.assert_page_matches_api()
        saved = {record["employee_no"]: record for record in records}
        self.assertIn("E701", saved)
        self.assertEqual(saved["E701"]["name"], NEW_EMPLOYEE_SAVED["name"])
        self.assertEqual(saved["E701"]["status"], "在职")
        self.assertEqual(
            saved["E701"]["effective_date"], NEW_EMPLOYEE_FORM["effective_date"]
        )

    def test_02_duplicate_employee_no_shows_error_and_keeps_everything(self):
        """重复工号：中文错误提示、输入全部保留、列表与原档案均不被改动。"""
        rows_before = read_list_rows()
        records_before = api_employees()

        # 用“ a007 ”（忽略大小写与首尾空白后与 A007 冲突）提交另一份完整档案。
        submit_form_and_wait_error(DUPLICATE_FORM)

        # 明确的工号已存在中文提示。
        form = read_form_state()
        self.assertFalse(form["error_hidden"], "工号冲突应显示错误提示")
        self.assertIn("工号", form["error_text"])
        self.assertIn("已存在", form["error_text"])

        # 刚填写的所有内容原样保留在表单中，包括首尾空白与选填联系方式。
        for field in FORM_FIELD_NAMES:
            self.assertEqual(
                form[field],
                DUPLICATE_FORM[field],
                f"提交失败后 {field} 应保留用户刚填写的内容",
            )

        # 页面列表不新增失败提交的员工，原 A007 行内容不被替换。
        rows_after = read_list_rows()
        self.assertEqual(rows_after, rows_before, "失败提交后页面列表不应变化")
        self.assertIn(expected_row(self.original_a007), rows_after)
        for row in rows_after:
            self.assertNotIn(DUPLICATE_FORM["name"], row)
            self.assertNotIn(DUPLICATE_FORM["department"], row)
            self.assertNotIn(DUPLICATE_FORM["phone"], row)
            self.assertNotIn(DUPLICATE_FORM["email"], row)

        # 接口查询同样确认：原档案逐字段保持原样，失败内容没有落库。
        records_after = api_employees()
        self.assertEqual(records_after, records_before, "失败提交不得改动任何已保存记录")
        self.assertIn(self.original_a007, records_after)
        self.assertFalse(
            any(record["name"] == DUPLICATE_FORM["name"] for record in records_after),
            "失败提交的姓名不得出现在员工列表中",
        )
        self.assertFalse(
            any(
                (record["employee_no"] or "").strip().lower() == "a007"
                and record["id"] != self.original_a007["id"]
                for record in records_after
            ),
            "失败提交不得以新记录形式占用 a007 工号",
        )
        self.assert_page_matches_api()

    def test_03_retry_with_unused_employee_no_succeeds(self):
        """只改工号为未使用值后再次保存：新增成功、原档案不变、错误消失。"""
        # 保证起点是“失败表单”状态：重新提交一次重复工号并确认错误提示。
        # （若 test_02 已运行，表单中仍是这些内容，重填结果相同。）
        submit_form_and_wait_error(DUPLICATE_FORM)
        form = read_form_state()
        self.assertFalse(form["error_hidden"])

        # 用户只把工号改为尚未使用的 E703，其余内容保持失败表单中的原样。
        _evaluate(
            "document.getElementById('emp-form').elements['employee_no'].value="
            f"{json.dumps(RETRY_EMPLOYEE_NO)};true"
        )
        click_save()

        # 新增为另一名员工，无需手动刷新。
        wait_for_list_row(RETRY_EMPLOYEE_NO, "改号后的新员工出现在员工列表")

        # 错误提示消失，表单清空。
        form = read_form_state()
        self.assertTrue(form["error_hidden"], "保存成功后错误提示应消失")
        for field in FORM_FIELD_NAMES:
            self.assertEqual(form[field], "", f"保存成功后 {field} 应恢复为空")

        # 新档案按失败表单中保留的内容（去空白后）展示；原 A007 档案保持原样。
        expected_new_row = [
            RETRY_EMPLOYEE_NO,
            DUPLICATE_FORM["name"],
            DUPLICATE_FORM["department"],
            DUPLICATE_FORM["position"],
            "在职",
            DUPLICATE_FORM["effective_date"],
            f"{DUPLICATE_FORM['phone']} · {DUPLICATE_FORM['email']}",
        ]
        rows_after = read_list_rows()
        self.assertIn(expected_new_row, rows_after)
        self.assertIn(expected_row(self.original_a007), rows_after)
        self.assertEqual(len(rows_after), 4, "应新增为独立员工，而不是覆盖原档案")

        # 最终页面与接口查询结果对应：原档案未被替换，新档案确实已保存。
        records = self.assert_page_matches_api()
        self.assertIn(self.original_a007, records, "原 A007 档案必须保持原样")
        by_no = {record["employee_no"]: record for record in records}
        self.assertIn(RETRY_EMPLOYEE_NO, by_no, "改号后的档案应已真正保存")
        self.assertNotEqual(
            by_no[RETRY_EMPLOYEE_NO]["id"],
            self.original_a007["id"],
            "改号重试应创建独立的新员工记录",
        )
        self.assertEqual(by_no[RETRY_EMPLOYEE_NO]["name"], DUPLICATE_FORM["name"])
        self.assertEqual(by_no[RETRY_EMPLOYEE_NO]["status"], "在职")
        self.assertEqual(
            by_no["A007"]["name"],
            ORIGINAL_A007["name"],
            "原档案姓名不得被重试提交替换",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
