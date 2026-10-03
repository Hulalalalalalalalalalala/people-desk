#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""特殊符号档案文字的端到端界面回归测试。

现有建档功能对姓名、工号、部门、岗位及联系方式只做去掉首尾空白的处理，
不禁止尖括号、引号或与号。本模块守住这一行为，防止后续调整页面时走向
两个极端：把填写内容当成网页内容渲染（注入元素、执行事件属性），或者
为了避免显示问题而删改、转义档案原文。

覆盖完整使用过程：从首页填写并保存一名特殊文字档案的员工，到员工列表
显示该档案，再通过员工列表接口读取同一记录：

1. 档案文字为中文与特殊符号混合：部门“研发 & 支持”、岗位“<培训负责人>”、
   姓名含中英文引号，并混入看起来像网页标签、带事件属性的文字
   （`<img src=x onerror=...>`）；工号、电话、邮箱同样含特殊符号。
   其余必填项合法，工号未被使用，生效日期为正常有效日期。
2. 保存仍应成功：接口中对应字段保留去掉首尾空白后的原文——不是显示用
   的转义文字，不遗漏符号，不改变内部空格，也不混入其他员工的信息；
   电话、邮箱不做格式校验，含特殊符号的字符串不得被新增限制拒绝。
3. 首页上这些内容只出现在对应员工的单元格内：用户读到完整原文；尖括号
   内容不变成新增页面元素，事件属性不执行行为，不插入额外行列、控件或
   提示；列顺序与员工对应关系保持正确；联系方式按电话在前、邮箱在后，
   两项都有内容时使用现有“ · ”分隔符。
4. 列表中同时保留一名普通档案员工：特殊文字的显示不影响另一行的姓名、
   部门和联系方式；两名员工在页面及接口中都各自只出现一次，保留各自
   编号与保存后的资料。

本模块不改变已有工号唯一性、生效日期校验或成功保存后的表单清空功能；
相关回归继续由 test_employee_no_uniqueness.py、
test_effective_date_validation.py 与 test_homepage_employee_form.py 覆盖。

运行方式（需要 Playwright 与本机 Chrome）：

    .venv/bin/python test_special_text_display.py
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
LIST_HEADERS = ("工号", "姓名", "部门", "岗位", "状态", "生效日期", "联系方式")

# 普通档案员工：通过接口预先建档，内容不含任何特殊符号，
# 用于证明特殊文字行的显示不会波及相邻行。
NORMAL = {
    "employee_no": "N100",
    "name": "普通档案·王五",
    "department": "行政部",
    "position": "行政专员",
    "effective_date": "2026-09-01",
    "phone": "13500000000",
    "email": "normal.n100@example.com",
}

# 特殊文字档案：各字段刻意带首尾空白（保存时只去掉首尾空白），
# 内部中文与特殊符号混合，内部空格必须原样保留。
# - 工号含尖括号与与号；
# - 姓名含中文引号、英文双引号、单引号，以及看起来像网页标签、
#   带事件属性的文字（若被当成 HTML 渲染会置 window.__xssFired）；
# - 部门为“研发 & 支持”；岗位为“<培训负责人>”；
# - 电话、邮箱含特殊符号（两者均不校验格式，不得因此被拒）。
SPECIAL = {
    "employee_no": " S<7&1> ",
    "name": " 张“引号”\"三'四' <img src=x onerror=\"window.__xssFired=1\"> ",
    "department": " 研发 & 支持 ",
    "position": " <培训负责人> ",
    "effective_date": "2026-10-01",
    "phone": " 138\"<电话>&转分机 ",
    "email": " special\"<xss>@example.com ",
}

# 保存后应为去掉首尾空白的原文（内部空格与符号一个不少）。
SAVED = {name: value.strip() for name, value in SPECIAL.items()}
SAVED["effective_date"] = SPECIAL["effective_date"]

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
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-xss-test-")
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


def list_employees_raw(base_url):
    """查询员工列表接口，返回未解析的响应原文（用于核对未被转义）。"""
    req = urllib.request.Request(base_url + "/api/employees", method="GET")
    with urllib.request.urlopen(req, timeout=10) as resp:
        assert resp.status == 200, f"列表查询失败: {resp.status}"
        return resp.read().decode("utf-8")


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
    """读取表单当前所有字段的值（不做任何加工）。"""
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


def wait_list_has_employee_no(page, employee_no):
    """等待列表第一列出现指定工号（提交成功后列表异步刷新的完成信号）。"""
    page.wait_for_function(
        """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
            .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
        arg=employee_no,
        timeout=10000,
    )


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class SpecialTextDisplayTest(unittest.TestCase):
    """特殊符号档案：首页建档 → 列表显示 → 接口读取，全程按普通文字对待。"""

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
        # 若带事件属性的文字被当成网页内容执行，或页面弹出提示，此处记录。
        self.dialogs = []
        self.page.on("dialog", lambda dialog: self.dialogs.append(dialog.message))

    def test_special_text_saved_and_rendered_as_plain_text(self):
        # ---- 准备：普通档案员工通过接口先行建档 ----
        status, normal = request(self.base_url, "POST", "/api/employees", NORMAL)
        self.assertEqual(status, 201, f"普通档案应建档成功: {normal}")

        open_homepage(self.page, self.base_url)
        self.assertEqual(
            list_rows(self.page),
            [expected_row(normal)],
            "提交前列表应只有普通档案员工",
        )

        # 在页面里打标记：若提交后发生整页刷新，标记会丢失。
        self.page.evaluate("() => { window.__stayOnSamePage = 'yes'; }")

        # ---- 第一步：从首页填写并保存特殊文字档案，保存应成功 ----
        fill_form(self.page, SPECIAL)
        self.page.click("#emp-form button[type=submit]")
        wait_list_has_employee_no(self.page, SAVED["employee_no"])

        self.assertEqual(
            self.page.evaluate("() => window.__stayOnSamePage"),
            "yes",
            "列表更新不应伴随整页刷新",
        )
        # 既有行为不变：保存成功后表单清空、错误提示不显示。
        self.assertEqual(
            read_form(self.page),
            {name: "" for name in FORM_FIELDS},
            "提交成功后表单所有字段都应恢复为空",
        )
        state = error_state(self.page)
        self.assertTrue(state["hidden"], f"合法的特殊文字不应触发错误提示: {state['text']!r}")

        # ---- 第二步：接口读取同一记录，字段为去首尾空白后的原文 ----
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), 2, "接口应恰好返回两名员工")
        self.assertEqual(employees[0], normal, "普通员工档案必须保持原样")
        special = employees[1]
        self.assertNotEqual(special["id"], normal["id"], "两名员工编号必须不同")

        # 逐字段核对：原文保留（含内部空格与全部符号），不是转义文字。
        for field in ("employee_no", "name", "department", "position", "phone", "email"):
            self.assertEqual(
                special[field],
                SAVED[field],
                f"接口字段 {field} 应保留去掉首尾空白后的原文",
            )
        self.assertEqual(special["effective_date"], SPECIAL["effective_date"])
        self.assertEqual(special["status"], "在职")

        # 响应原文层面同样未被转义：没有 &lt; &gt; &amp; &quot; 之类的
        # 显示用转义文字，特殊符号原样出现在 JSON 里（姓名含英文双引号，
        # 按 JSON 转义后的形式核对）。
        raw = list_employees_raw(self.base_url)
        json_encoded_name = json.dumps(SAVED["name"], ensure_ascii=False)[1:-1]
        self.assertIn(json_encoded_name, raw)
        self.assertIn(SAVED["department"], raw)
        self.assertIn(SAVED["position"], raw)
        for escaped in ("&lt;", "&gt;", "&amp;", "&quot;", "&#39;"):
            self.assertNotIn(escaped, raw, f"接口不应返回转义文字 {escaped!r}")

        # 两名员工在接口中各自只出现一次。
        self.assertEqual(
            [row["employee_no"] for row in employees].count(SAVED["employee_no"]), 1
        )
        self.assertEqual(
            [row["employee_no"] for row in employees].count(NORMAL["employee_no"]), 1
        )

        # ---- 第三步：页面列表把特殊文字当普通文字显示 ----
        headers = self.page.evaluate(
            """() => Array.from(
                document.querySelectorAll('#list thead th'),
                (th) => th.textContent
            )"""
        )
        self.assertEqual(headers, list(LIST_HEADERS), "列表列顺序与表头必须保持原样")

        rows = list_rows(self.page)
        self.assertEqual(len(rows), 2, "页面列表应恰好两行，不得插入额外行")
        for row in rows:
            self.assertEqual(len(row), len(LIST_HEADERS), "每行单元格数必须与表头一致")

        # 普通员工行不受特殊文字影响：姓名、部门、联系方式逐项原样。
        self.assertEqual(rows[0], expected_row(normal))
        self.assertEqual(rows[0][1], NORMAL["name"])
        self.assertEqual(rows[0][2], NORMAL["department"])
        self.assertEqual(rows[0][6], f"{NORMAL['phone']} · {NORMAL['email']}")

        # 特殊员工行：每个单元格的可见文字与保存原文完全一致，
        # 联系方式按电话在前、邮箱在后，以现有分隔符连接。
        self.assertEqual(rows[1], expected_row(special))
        self.assertEqual(
            rows[1],
            [
                SAVED["employee_no"],
                SAVED["name"],
                SAVED["department"],
                SAVED["position"],
                "在职",
                SAVED["effective_date"],
                f"{SAVED['phone']} · {SAVED['email']}",
            ],
        )

        # 特殊文字只出现在对应员工的单元格内：不混入普通员工行，
        # 两名员工在页面上各自只出现一次。
        for cell in rows[0]:
            self.assertNotIn(SAVED["name"], cell)
            self.assertNotIn(SAVED["department"], cell)
            self.assertNotIn(SAVED["position"], cell)
        self.assertEqual(
            [row[0] for row in rows].count(SAVED["employee_no"]), 1
        )
        self.assertEqual(
            [row[0] for row in rows].count(NORMAL["employee_no"]), 1
        )

        # 尖括号内容不变成新增页面元素：列表内没有多出的 img/script/
        # iframe 等元素，所有单元格只含纯文本（没有任何子元素）。
        dom_check = self.page.evaluate(
            """() => {
                const list = document.getElementById('list');
                const cells = Array.from(list.querySelectorAll('td'));
                return {
                    injected: list.querySelectorAll('img, script, iframe, object, embed, video, audio, svg').length,
                    cellsWithChildren: cells.filter((td) => td.childElementCount > 0).length,
                    tables: list.querySelectorAll('table').length,
                    buttons: document.querySelectorAll('button').length,
                    xssFired: window.__xssFired === undefined ? 'no' : String(window.__xssFired),
                };
            }"""
        )
        self.assertEqual(dom_check["injected"], 0, "特殊文字不得渲染成新的页面元素")
        self.assertEqual(dom_check["cellsWithChildren"], 0, "单元格内不得出现子元素")
        self.assertEqual(dom_check["tables"], 1, "不得插入额外的表格")
        self.assertEqual(dom_check["buttons"], 1, "不得插入额外的控件")
        self.assertEqual(dom_check["xssFired"], "no", "带事件属性的文字不得执行行为")
        self.assertEqual(self.dialogs, [], "特殊文字不得触发弹窗提示")

        # 错误提示保持隐藏：没有因特殊文字而冒出额外提示。
        state = error_state(self.page)
        self.assertTrue(state["hidden"], f"页面上不应出现额外提示: {state['text']!r}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
