#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""员工档案特殊文字的端到端自动化回归测试。

建档功能对姓名、工号、部门、岗位及联系方式只做“去掉首尾空白”，并不禁止
尖括号、引号或与号；本模块守住这个已有行为：无论填写内容包含怎样的
特殊符号，服务端都按普通文字保存原文，首页也必须把它们作为普通文字
显示——既不能在后续调整页面时把填写内容当成网页内容解析（插入元素、
执行事件），也不能为了避免显示问题而拒绝、删改档案或把原文替换成
显示用的转义文字。

覆盖从首页填写并保存一名员工，到员工列表显示该档案、再通过现有列表
接口读取同一记录的完整使用过程（Playwright 驱动本机 Chrome，headless；
缺少 Playwright 时界面用例整类跳过，接口级用例始终运行）：

接口级（真实 HTTP 进程，仅标准库）：
1. 含中文与特殊符号混合内容（部门“研发 & 支持”、岗位“<培训负责人>”、
   姓名中的引号、看起来像网页标签并带事件属性的文字、含特殊符号的
   电话与邮箱）的档案提交后仍返回 201；保存响应与列表接口中的对应字段
   都是去掉首尾空白后的原文：不转义成 &amp;/&lt;/&quot; 之类的显示文字，
   不遗漏符号，不改变内部空格。
2. 特殊档案与一名普通档案在列表中各自只出现一次，保留各自编号与资料，
   互不混入对方的姓名、部门或联系方式。
3. 已有规则不改变：工号忽略大小写判重（409 + field），日期非法仍 400，
   纯空白必填字段仍 400，失败提交不新增、不留残缺档案。

界面级（首页实际填写表单并点击“保存档案”）：
4. 保存成功后表单清空、错误提示不显示；特殊员工无需手动刷新整页即
   出现在列表中，页面单元格的 textContent 与接口原文逐字一致，用户能
   读到完整原文（含尖括号、引号、与号与内部空格）。
5. 尖括号里的内容不会变成新增页面元素：特殊员工所在行的每个单元格都
   只有文本节点，列表主体内除表格行列外没有任何多余元素；带事件属性的
   文字不执行任何行为（无弹窗、无事件标记，悬停并点击该单元格后依旧
   不触发）；不插入额外行列、控件或提示。
6. 页面原有列顺序（工号/姓名/部门/岗位/状态/生效日期/联系方式）与
   员工对应关系保持正确；联系方式仍按电话在前、邮箱在后、以现有
   分隔符“ · ”连接，含特殊符号的电话邮箱不被新增限制拒绝。
7. 同列表中的普通档案行逐字段保持原样，特殊文字只出现在特殊员工自己的
   单元格内；两名员工在页面与接口中都各自只出现一次。重新打开首页
   （从接口重新加载并渲染）后以上结论仍然成立。

运行方式：

    python3 test_employee_special_text.py            # 仅接口级用例
    .venv/bin/python test_employee_special_text.py   # 含界面级用例（需 Playwright）

工号唯一性、生效日期校验、表单清空与旧档案展示的回归保障继续保留在
test_employee_no_uniqueness.py、test_effective_date_validation.py、
test_homepage_employee_form.py 与 test_legacy_employee_records.py，
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
except ImportError:  # pragma: no cover - 环境缺少 playwright 时界面用例整类跳过
    sync_playwright = None

APP_PATH = Path(__file__).resolve().parent / "app.py"

FORM_FIELDS = (
    "employee_no", "name", "department", "position",
    "effective_date", "phone", "email",
)

# 所有可能含特殊符号的文字字段：接口中必须逐字保留原文。
TEXT_FIELDS = ("employee_no", "name", "department", "position", "phone", "email")

# 显示用的 HTML 转义形态绝不能出现在接口数据里（它们只是页面渲染手段）。
HTML_ESCAPES = ("&amp;", "&lt;", "&gt;", "&quot;", "&#39;")

# 页面表头顺序：工号/姓名/部门/岗位/状态/生效日期/联系方式。
EXPECTED_HEADERS = ["工号", "姓名", "部门", "岗位", "状态", "生效日期", "联系方式"]

# ---------------------------------------------------------------------------
# 测试数据
# ---------------------------------------------------------------------------

# 特殊文字档案：刻意带首尾空白（验证保存值仍是“仅去掉首尾空白”后的原文），
# 内容覆盖：与号、尖括号（含看起来像网页标签、带事件属性的文字）、
# 单双引号、内部空格，以及不做格式校验、含特殊符号的电话与邮箱。
SPECIAL_EMPLOYEE = {
    "employee_no": " X-501 ",
    "name": ' 张"三\'四 ',
    "department": " 研发 & 支持 ",
    # <培训负责人> 是普通文字；<img ... onerror> 与 <b onmouseover> 看似
    # 网页标签并带事件属性，任何一环把填写内容当成 HTML 解析都会暴露。
    "position": (
        ' <培训负责人> / '
        '<img src=x501 onerror=window.__xssFired="fired"> / '
        "<b onmouseover=window.__xssHover=1>加粗</b> "
    ),
    "effective_date": "2026-09-15",
    "phone": ' 电话<热线> "501" & 分机 9 ',
    "email": " 'x501'@a<b>.test&x ",
}
SPECIAL_EMPLOYEE_NO = "X-501"
SPECIAL_SAVED = {key: value.strip() for key, value in SPECIAL_EMPLOYEE.items()}

# 普通档案：与特殊档案内容完全不同，用于确认特殊文字的显示不会影响
# 另一行的姓名、部门和联系方式。
PLAIN_EMPLOYEE = {
    "employee_no": "X-500",
    "name": "王平凡",
    "department": "行政部",
    "position": "行政专员",
    "effective_date": "2026-08-20",
    "phone": "13500005000",
    "email": "plain.x500@example.com",
}
PLAIN_EMPLOYEE_NO = "X-500"

# 特殊档案中绝不允许出现在普通员工行内的标记片段（含跨单元格泄漏检测）。
SPECIAL_MARKERS = (
    SPECIAL_SAVED["name"],
    SPECIAL_SAVED["department"],
    "<培训负责人>",
    "<img",
    "onerror",
    "onmouseover",
    "x501",
    "电话<热线>",
    "a<b>.test",
)

# ---------------------------------------------------------------------------
# 服务进程与 HTTP 帮助函数（与其他回归测试模块保持一致：真实进程 + 标准库）
# ---------------------------------------------------------------------------

def start_server():
    """以全新数据目录启动真实服务进程，返回 (base_url, proc, tmpdir)。"""
    tmpdir = tempfile.mkdtemp(prefix="peopledesk-special-test-")
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


def assert_special_recipe_saved(testcase, record):
    """接口记录必须逐字保留“仅去掉首尾空白”后的特殊文字原文。"""
    testcase.assertEqual(record["status"], "在职")
    testcase.assertEqual(record["effective_date"], SPECIAL_EMPLOYEE["effective_date"])
    for field in TEXT_FIELDS:
        testcase.assertEqual(
            record[field], SPECIAL_SAVED[field],
            f"{field} 必须保留去掉首尾空白后的原文，不能删改符号或改变内部空格",
        )
        for escaped in HTML_ESCAPES:
            testcase.assertNotIn(
                escaped, record[field],
                f"{field} 不能混入显示用的转义文字 {escaped}: {record[field]!r}",
            )


# ---------------------------------------------------------------------------
# 接口级回归
# ---------------------------------------------------------------------------

class SpecialTextApiTest(unittest.TestCase):
    """特殊文字通过接口建档、读回，以及与普通档案共存的公开行为。"""

    def setUp(self):
        # 每个用例使用独立服务进程与数据目录，用例间互不占用工号。
        self.base_url, self.proc, self.tmpdir = start_server()

    def tearDown(self):
        stop_server(self.proc, self.tmpdir)

    def test_special_characters_saved_as_plain_text_and_read_back(self):
        # 1. 含尖括号、引号、与号及事件属性文字的档案仍应建档成功。
        status, created = request(self.base_url, "POST", "/api/employees", SPECIAL_EMPLOYEE)
        self.assertEqual(status, 201, f"特殊文字档案不应被拒绝: {created}")
        self.assertIsInstance(created["id"], int)
        special_id = created["id"]
        assert_special_recipe_saved(self, created)

        # 再建一名普通档案，确认特殊文字不会妨碍或污染后续建档。
        status, plain = request(self.base_url, "POST", "/api/employees", PLAIN_EMPLOYEE)
        self.assertEqual(status, 201, f"普通档案应建档成功: {plain}")
        self.assertNotEqual(plain["id"], special_id)

        # 2. 通过现有列表接口读回：两名员工各自只出现一次，按建档编号升序
        #    （特殊档案先建），特殊记录逐字是去空白后的原文，普通记录逐字
        #    是其填写内容。
        employees = list_employees(self.base_url)
        self.assertEqual(len(employees), 2, "两名员工都应恰好保存一次")
        self.assertEqual(
            [row["employee_no"] for row in employees],
            [SPECIAL_EMPLOYEE_NO, PLAIN_EMPLOYEE_NO],
        )
        ids = [row["id"] for row in employees]
        self.assertEqual(len(set(ids)), 2, "两名员工必须保留各自编号")

        special_rows = [row for row in employees if row["id"] == special_id]
        plain_rows = [row for row in employees if row["id"] == plain["id"]]
        self.assertEqual(len(special_rows), 1)
        self.assertEqual(len(plain_rows), 1)
        assert_special_recipe_saved(self, special_rows[0])
        self.assertEqual(
            plain_rows[0], plain, "普通档案应与建档响应逐字段一致"
        )

        # 3. 两行资料互不混入：特殊记录不含普通员工的姓名/部门/联系方式，
        #    普通记录也不沾任何特殊符号片段。
        special_record = special_rows[0]
        plain_record = plain_rows[0]
        for field in ("name", "department", "phone", "email"):
            self.assertNotEqual(special_record[field], plain_record[field])
            self.assertNotIn(plain_record[field], special_record[field])
        for marker in SPECIAL_MARKERS:
            for field in ("name", "department", "position", "phone", "email"):
                self.assertNotIn(
                    marker, plain_record[field],
                    f"特殊文字片段 {marker!r} 不得混入普通员工的 {field}",
                )

    def test_existing_validation_rules_unchanged_with_special_text(self):
        """工号唯一性、生效日期与必填校验不因特殊文字而改变。"""
        status, created = request(self.base_url, "POST", "/api/employees", SPECIAL_EMPLOYEE)
        self.assertEqual(status, 201, f"特殊文字档案应建档成功: {created}")
        baseline = list_employees(self.base_url)

        # 工号忽略大小写、忽略首尾空白判重：换成小写并加空白仍冲突，
        # 且不能覆盖已有特殊档案。
        duplicate = dict(SPECIAL_EMPLOYEE, employee_no=" x-501 ", name="另一个人")
        status, body = request(self.base_url, "POST", "/api/employees", duplicate)
        self.assertEqual(status, 409)
        self.assertEqual(body.get("field"), "employee_no")
        self.assertIn("工号", body.get("error", ""))
        self.assertIn("已存在", body.get("error", ""))

        # 日期非法：即便其余文字字段含特殊符号，仍是 400 且指向生效日期。
        bad_date = dict(SPECIAL_EMPLOYEE, employee_no="X-510", effective_date="2026-02-30")
        status, body = request(self.base_url, "POST", "/api/employees", bad_date)
        self.assertEqual(status, 400)
        self.assertEqual(body.get("field"), "effective_date")

        # 纯空白必填字段仍被拒绝（建档只去首尾空白，去空白后必须有内容）。
        blank_position = dict(SPECIAL_EMPLOYEE, employee_no="X-511", position="   ")
        status, body = request(self.base_url, "POST", "/api/employees", blank_position)
        self.assertEqual(status, 400)
        self.assertEqual(body.get("field"), "position")

        # 失败提交不新增、不留残缺档案，特殊档案保持原样。
        employees = list_employees(self.base_url)
        self.assertEqual(
            employees, baseline,
            "被拒绝的提交不得新增记录，也不得改动已保存的特殊档案",
        )
        assert_special_recipe_saved(
            self, next(row for row in employees if row["employee_no"] == SPECIAL_EMPLOYEE_NO)
        )


# ---------------------------------------------------------------------------
# 界面级回归（需要 Playwright 与本机 Chrome）
# ---------------------------------------------------------------------------

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


def fill_form(page, values):
    """按输入框 name 逐个填写，值原样送入（含首尾空白），与用户键入一致。"""
    for name, value in values.items():
        page.fill(f"#emp-form input[name={name}]", value)


def read_form(page):
    """读取表单当前所有字段的值（用于核对提交成功后的表单清空）。"""
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


def list_rows(page):
    """读取首页员工列表当前渲染出的全部数据行（文本与页面显示一致）。"""
    return page.evaluate(
        """() => Array.from(
            document.querySelectorAll('#list tbody tr'),
            (tr) => Array.from(tr.cells, (td) => td.textContent)
        )"""
    )


def table_shape(page):
    """返回列表的结构信息：表头、行列数、主体内出现过的标签名与控件数。"""
    return page.evaluate(
        """() => {
            const list = document.getElementById('list');
            const rows = Array.from(list.querySelectorAll('tbody tr'));
            return {
                tableCount: list.querySelectorAll('table').length,
                headers: Array.from(list.querySelectorAll('thead th'), (th) => th.textContent),
                rowCount: rows.length,
                cellCounts: rows.map((tr) => tr.cells.length),
                bodyTagNames: Array.from(list.querySelectorAll('tbody *'), (el) => el.tagName),
                controlCount: list.querySelectorAll(
                    'button,input,a,select,textarea,img,svg,script,iframe'
                ).length,
            };
        }"""
    )


def row_dom(page, employee_no):
    """按工号定位一行，返回各单元格的文本、innerHTML 与后代元素数量。"""
    return page.evaluate(
        """(no) => {
            const tr = Array.from(document.querySelectorAll('#list tbody tr'))
                .find((row) => row.cells.length > 0 && row.cells[0].textContent === no);
            if (!tr) return null;
            return {
                texts: Array.from(tr.cells, (td) => td.textContent),
                htmls: Array.from(tr.cells, (td) => td.innerHTML),
                elementCounts: Array.from(tr.cells, (td) => td.querySelectorAll('*').length),
            };
        }""",
        employee_no,
    )


def open_homepage(page, base_url):
    """打开首页并等待首次列表加载完成（出现表格或空态提示）。"""
    page.goto(base_url + "/")
    page.wait_for_selector("#list table, #list .empty", timeout=10000)


@unittest.skipUnless(sync_playwright, "需要 playwright 与本机 Chrome 才能执行界面回归")
class SpecialTextHomepageTest(unittest.TestCase):
    """首页建档后，特殊文字只能作为纯文本出现在对应员工的单元格内。"""

    @classmethod
    def setUpClass(cls):
        cls.base_url, cls.proc, cls.tmpdir = start_server()

    @classmethod
    def tearDownClass(cls):
        stop_server(cls.proc, cls.tmpdir)

    def setUp(self):
        self.context = _browser.new_context()
        self.page = self.context.new_page()
        # 任何真被解析执行的注入行为若调用 alert，都会在这里留下记录。
        self.dialogs = []
        self.page.on("dialog", self._handle_dialog)
        self.addCleanup(self.context.close)

    def _handle_dialog(self, dialog):
        self.dialogs.append(dialog.message)
        dialog.dismiss()

    def _assert_list_is_plain_text(self, employees):
        """对当前页面列表做完整的纯文本展示与结构安全断言。"""
        special = next(row for row in employees if row["employee_no"] == SPECIAL_EMPLOYEE_NO)
        plain = next(row for row in employees if row["employee_no"] == PLAIN_EMPLOYEE_NO)
        special_expected = expected_row(special)
        plain_expected = expected_row(plain)

        # 列顺序固定为 工号/姓名/部门/岗位/状态/生效日期/联系方式。
        shape = table_shape(self.page)
        self.assertEqual(shape["tableCount"], 1, "特殊文字不得插入额外表格")
        self.assertEqual(shape["headers"], EXPECTED_HEADERS, "页面原有列顺序必须保持")
        self.assertEqual(shape["rowCount"], 2, "只能有两名员工两行，不得插入额外行")
        self.assertEqual(shape["cellCounts"], [7, 7], "每行必须仍是 7 个单元格")
        self.assertEqual(
            set(shape["bodyTagNames"]), {"TR", "TD"},
            "列表主体内除表格行列外不得出现任何其他元素（尖括号内容不能变成元素）",
        )
        self.assertEqual(shape["controlCount"], 0, "列表中不得插入控件或提示元素")

        # 行文本与接口记录逐行对应：用户读到的是完整原文。
        rows = list_rows(self.page)
        self.assertEqual(rows, [plain_expected, special_expected])
        self.assertEqual(rows[1], special_expected)

        # 两名员工在页面上各自只出现一次（工号与姓名各出现一次）。
        list_text = self.page.evaluate("() => document.getElementById('list').textContent")
        self.assertEqual(list_text.count(SPECIAL_EMPLOYEE_NO), 1)
        self.assertEqual(list_text.count(PLAIN_EMPLOYEE_NO), 1)
        self.assertEqual(list_text.count(SPECIAL_SAVED["name"]), 1)
        self.assertEqual(list_text.count(PLAIN_EMPLOYEE["name"]), 1)

        special_dom = row_dom(self.page, SPECIAL_EMPLOYEE_NO)
        self.assertIsNotNone(special_dom, "特殊员工应在列表中")
        plain_dom = row_dom(self.page, PLAIN_EMPLOYEE_NO)
        self.assertIsNotNone(plain_dom, "普通员工应在列表中")

        # 特殊员工行：单元格文本逐字等于原文（引号、与号、尖括号、内部空格
        # 都在），且每个单元格都没有任何子元素——只有文本节点。
        self.assertEqual(special_dom["texts"], special_expected)
        self.assertEqual(
            special_dom["elementCounts"], [0] * 7,
            "尖括号里的内容不能变成新增页面元素，单元格内只能有文本",
        )
        # innerHTML 必须是转义后的普通文本：与号、尖括号以实体形态存在，
        # 不存在任何真实标签或属性；单双引号作为普通字符留在文本节点中
        # （浏览器序列化文本节点时引号可直接呈现，关键是不能成为属性）。
        name_html, dept_html, position_html = (
            special_dom["htmls"][1], special_dom["htmls"][2], special_dom["htmls"][3]
        )
        contact_html = special_dom["htmls"][6]
        self.assertIn('张"三\'四', name_html, "姓名中的单双引号必须按普通文本保留")
        self.assertIn("研发 &amp; 支持", dept_html, "与号必须按文本转义显示")
        self.assertIn("&lt;培训负责人&gt;", position_html, "尖括号必须按文本显示")
        self.assertIn("&lt;img", position_html, "看似标签的文字不能成为真实 img 元素")
        self.assertNotIn("<img", position_html)
        self.assertNotIn("<b ", position_html, "带事件属性的文字不能成为真实元素")
        # innerHTML 里即便能看到 onerror/onmouseover 字样，也只是被转义文本
        # 的一部分；真正要守住的是不存在携带这些事件属性的真实元素节点。
        self.assertEqual(
            self.page.evaluate(
                "() => document.querySelectorAll('#list [onerror],#list [onmouseover]').length"
            ),
            0,
            "带事件属性的文字不得成为真实事件属性",
        )
        self.assertIn("&amp; 分机", contact_html, "电话中的与号必须按文本显示")
        self.assertIn("a&lt;b&gt;.test&amp;x", contact_html, "邮箱中的尖括号与与号必须按文本显示")

        # 联系方式顺序：电话在前、邮箱在后，使用现有分隔符“ · ”，
        # 两项特殊符号内容都完整保留。
        self.assertEqual(
            special_dom["texts"][6],
            f'{SPECIAL_SAVED["phone"]} · {SPECIAL_SAVED["email"]}',
            "联系方式必须按电话在前、邮箱在后、以“ · ”连接显示原文",
        )
        self.assertEqual(
            plain_dom["texts"][6],
            f'{PLAIN_EMPLOYEE["phone"]} · {PLAIN_EMPLOYEE["email"]}',
        )

        # 特殊文字只能出现在特殊员工的单元格内：普通员工行逐字段保持原样，
        # 不含任何特殊片段；特殊员工行也不混入普通员工的资料。
        self.assertEqual(plain_dom["texts"], plain_expected, "普通档案行必须逐字段保持原样")
        for cell_text in plain_dom["texts"]:
            for marker in SPECIAL_MARKERS:
                self.assertNotIn(
                    marker, cell_text,
                    f"特殊文字片段 {marker!r} 不得泄漏到普通员工单元格",
                )
        for marker in (PLAIN_EMPLOYEE["name"], PLAIN_EMPLOYEE["department"],
                       PLAIN_EMPLOYEE["phone"], PLAIN_EMPLOYEE["email"]):
            for cell_text in special_dom["texts"]:
                self.assertNotIn(marker, cell_text, "特殊员工行不得混入普通员工资料")

        # 事件没有执行：无弹窗；注入代码试图写入的标记保持未定义。
        # 悬停并点击特殊岗位单元格后再确认一次（覆盖 onmouseover 类行为）。
        position_cell = self.page.locator("#list tbody tr").filter(
            has=self.page.locator("td:first-child", has_text=SPECIAL_EMPLOYEE_NO)
        ).locator("td:nth-child(4)")
        position_cell.hover()
        position_cell.click()
        self.assertEqual(self.dialogs, [], "特殊文字不得触发任何弹窗行为")
        self.assertIsNone(
            self.page.evaluate("() => window.__xssFired || null"),
            "带 onerror 的文字绝不能执行",
        )
        self.assertIsNone(
            self.page.evaluate("() => window.__xssHover || null"),
            "带 onmouseover 的文字绝不能执行",
        )

    def test_special_text_displayed_as_plain_text_without_extra_elements(self):
        # 先通过接口放入一名普通档案，作为页面上的对照行。
        status, plain = request(self.base_url, "POST", "/api/employees", PLAIN_EMPLOYEE)
        self.assertEqual(status, 201, f"普通档案应建档成功: {plain}")

        open_homepage(self.page, self.base_url)
        self.assertEqual(list_rows(self.page), [expected_row(plain)])

        # 在页面里打标记：提交后若整页刷新，标记会丢失。
        self.page.evaluate("() => { window.__stayOnSamePage = 'yes'; }")

        # 通过首页表单填写并保存含特殊文字的档案（与真实用户键入一致，
        # 含首尾空白），保存仍应成功。
        fill_form(self.page, SPECIAL_EMPLOYEE)
        self.page.click("#emp-form button[type=submit]")
        self.page.wait_for_function(
            """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
            arg=SPECIAL_EMPLOYEE_NO,
            timeout=10000,
        )

        # 成功保存后的既有行为：表单全部清空、错误提示不显示、无整页刷新。
        self.assertEqual(
            read_form(self.page), {name: "" for name in FORM_FIELDS},
            "提交成功后表单所有字段都应恢复为空（含特殊文字字段）",
        )
        error_state = self.page.evaluate(
            """() => {
                const box = document.getElementById('form-error');
                return {hidden: box.hidden, text: box.textContent};
            }"""
        )
        self.assertTrue(error_state["hidden"], f"不应出现错误提示: {error_state['text']!r}")
        self.assertEqual(
            self.page.evaluate("() => window.__stayOnSamePage"), "yes",
            "列表更新不应伴随整页刷新",
        )

        # 接口读回同一记录：原文逐字保留，两名员工各自只出现一次。
        employees = list_employees(self.base_url)
        self.assertEqual(
            [row["employee_no"] for row in employees],
            [PLAIN_EMPLOYEE_NO, SPECIAL_EMPLOYEE_NO],
        )
        self.assertEqual(len({row["id"] for row in employees}), 2)
        assert_special_recipe_saved(
            self, next(row for row in employees if row["employee_no"] == SPECIAL_EMPLOYEE_NO)
        )

        # 首次异步渲染后的页面必须满足全部纯文本展示要求。
        self._assert_list_is_plain_text(employees)

        # 重新打开首页（完全从接口重新加载并渲染），结论必须仍然成立：
        # 特殊文字的安全展示不依赖单次内存状态。
        open_homepage(self.page, self.base_url)
        self.page.wait_for_function(
            """(no) => Array.from(document.querySelectorAll('#list tbody tr'))
                .some((tr) => tr.cells.length > 0 && tr.cells[0].textContent === no)""",
            arg=SPECIAL_EMPLOYEE_NO,
            timeout=10000,
        )
        self._assert_list_is_plain_text(employees)
        self.assertEqual(self.dialogs, [], "重新加载后特殊文字仍不得触发任何弹窗行为")


if __name__ == "__main__":
    unittest.main(verbosity=2)
