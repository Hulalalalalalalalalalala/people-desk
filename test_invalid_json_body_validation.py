#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""建档请求体本身不成立时的端到端自动化回归测试。

POST /api/employees 的校验顺序必须是：先把请求体按 UTF-8 读取并解析为
JSON，再确认最外层是对象，之后才进入工号、姓名等字段校验。现有测试主要
保障对象内部的字段，本模块通过真实 HTTP 进程（仅使用标准库）把“提交内容
本身不成立”时的结果也保护起来，沿用当前建档规则和错误响应：

1. 请求体为空、只有空白、JSON 残缺或语法错误、含无法按 UTF-8 解码的字节
   时，一律返回 400，响应仍是可按 UTF-8 读取的 JSON，中文原因固定为
   “请求体不是有效的 JSON 对象”，且不带 field；即使残缺内容中已经出现
   合法工号或姓名，也不得根据其中一部分内容保存档案。
2. 内容可以正常解析，但最外层是数组、字符串、数字、布尔值或 null 时，
   同样返回 400、响应为可读 JSON，中文原因固定为“请求体必须是 JSON
   对象”，同样不带 field。数组中即使装着完整员工对象（一个或多个），
   也不能被当作一次或多次建档请求接受。
3. 以上格式错误必须在字段校验与工号判重之前生效：错误内容里即使出现
   已存在的工号，也只能得到请求体格式错误（400），不能变成 409 工号
   重复提示或某个字段的填写提示。
4. 拒绝结果必须与档案数据一致：在已有员工记录的情况下，每次错误提交后
   的员工列表都与提交前完全相同，已有编号、姓名、部门和联系方式保持
   原样，不新增残缺记录；全部错误之后，服务仍能接受资料完整的正常对象，
   返回 201，保存后的员工可以从列表中查询到。
5. 保留对象与非对象之间的边界：空对象 {} 是合法 JSON 对象，必须进入
   现有必填字段校验，返回 400 并以 field=employee_no 指明缺少工号，
   不能与无法解析的内容使用同一种错误原因。
6. 正常对象中包含中文姓名和部门、带中文备注的联系方式或非邮箱格式的
   邮箱时仍正常保存并原样返回，本保障不增加任何文字、日期或联系方式
   限制。
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

APP_PATH = Path(__file__).resolve().parent / "app.py"

UNPARSEABLE_MESSAGE = "请求体不是有效的 JSON 对象"
NON_OBJECT_MESSAGE = "请求体必须是 JSON 对象"

# 字段级/唯一性错误文案中的字样，请求体格式错误绝不允许混入这些提示。
FIELD_ERROR_HINTS = ("不能为空", "必须是字符串", "已存在", "日期")

# 一份除请求体字节外均符合现有录入规则的完整员工对象，供数组场景使用。
def _complete_employee_json(employee_no, name):
    obj = {
        "employee_no": employee_no,
        "name": name,
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-10-01",
        "phone": f"电话·{employee_no}",
        "email": f"{employee_no.lower()}@example.com",
    }
    return json.dumps(obj, ensure_ascii=False).encode("utf-8")


# 请求体无法按 UTF-8 读取或无法解析为 JSON 的各类情形。
# 元素为 (raw_bytes_or_None, 说明)；None 表示完全不发送请求体。
UNPARSEABLE_BODIES = [
    (None, "完全没有请求体（无 Content-Length）"),
    (b"", "空请求体（Content-Length: 0）"),
    (b"   \t\r\n  ", "只有空白字节"),
    # 残缺 JSON：已经出现合法工号和中文姓名，但对象没有闭合。
    (
        b'{"employee_no": "J101", "name": "' + "张三".encode("utf-8") + b'"',
        "对象未闭合的残缺 JSON（已含合法工号与姓名）",
    ),
    (
        b'{"employee_no":"J102","name":"' + "李四".encode("utf-8") + b'",',
        "成员列表后只剩一个逗号就截断",
    ),
    (b'{"employee_no": "J103", "department":}', "字段值缺失的语法错误"),
    (
        b'{employee_no: "J104", name: "' + "王五".encode("utf-8") + b'"}',
        "键没有双引号，不是合法 JSON",
    ),
    (b'{"employee_no":"J105"} }', "多出一个右花括号"),
    (b"not-json-at-all", "完全不是 JSON 语法的文字"),
    (
        b'{"name": "' + "张三".encode("utf-8") + b'\xff"}',
        "字符串值中混入无法按 UTF-8 解码的字节（语法看似完整）",
    ),
    (b"\xff\xfe" + b'{"employee_no":"J202"}', "开头即含无法按 UTF-8 解码的字节"),
    (b"\xff", "整个请求体只有一个非法 UTF-8 字节"),
    (
        json.dumps({"employee_no": "J203", "name": "赵六"}, ensure_ascii=False)
        .encode("utf-8")[:-1]
        + b"\xc3\x28",
        "残缺 JSON 且末尾字节无法按 UTF-8 解码",
    ),
]

# 可以正常解析、但最外层不是对象的标量情形。
NON_OBJECT_SCALARS = [
    (b"[]", "空数组"),
    (b'"' + "张三".encode("utf-8") + b'"', "中文字符串"),
    (b'"just a string"', "英文字符串"),
    (b"123", "正整数"),
    (b"-3.14", "负数小数"),
    (b"true", "布尔真"),
    (b"false", "布尔假"),
    (b"null", "null"),
]

BASELINE_EMPLOYEE = {
    "employee_no": "J801",
    "name": "基线档案·赵一",
    "department": "人事部",
    "position": "HRBP",
    "effective_date": "2026-09-01",
    "phone": "13800000081",
    "email": "base.j801@example.com",
}

AFTER_BAD_BODIES_EMPLOYEE = {
    "employee_no": "J802",
    "name": "错误之后新建·孙二",
    "department": "市场部",
    "position": "招聘专员",
    "effective_date": "2026-10-03",
    "phone": "",
    "email": "",
}

CHINESE_OBJECT_EMPLOYEE = {
    "employee_no": "J901",
    "name": "陈 明",
    "department": "研发 平台部",
    "position": "后端工程师",
    "effective_date": "2026-10-04",
    "phone": "分机 9001（中文备注，内部有空格）",
    "email": "不是邮箱·也允许原样保存",
}

BASE_URL = None
_proc = None
_tmpdir = None


def _start_server():
    global BASE_URL, _proc, _tmpdir
    _tmpdir = tempfile.mkdtemp(prefix="peopledesk-body-test-")
    _proc = subprocess.Popen(
        [
            sys.executable,
            str(APP_PATH),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--data-dir",
            _tmpdir,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    deadline = time.time() + 10
    line = ""
    while time.time() < deadline:
        line = _proc.stdout.readline()
        if not line:
            if _proc.poll() is not None:
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
    global _proc, _tmpdir
    if _proc is not None and _proc.poll() is None:
        _proc.send_signal(signal.SIGTERM)
        try:
            _proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            _proc.kill()
            _proc.wait(timeout=5)
        if _proc.stdout is not None:
            _proc.stdout.close()
    _proc = None
    if _tmpdir is not None:
        shutil.rmtree(_tmpdir, ignore_errors=True)
        _tmpdir = None


def setUpModule():
    _start_server()


def tearDownModule():
    _stop_server()


def request(method, path, body=None):
    """发起 JSON 对象请求，返回 (status_code, json_body)。"""
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


def send_raw(raw, *, path="/api/employees", content_type="application/json"):
    """直接发送原始字节（raw 为 None 时完全不带请求体），返回
    (status_code, headers, body_bytes)，由调用方亲自验证响应如何被读取。"""
    headers = {"Content-Type": content_type}
    req = urllib.request.Request(
        BASE_URL + path, data=raw, headers=headers, method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, dict(resp.headers), resp.read()
    except urllib.error.HTTPError as err:
        return err.code, dict(err.headers), err.read()


def list_by_id():
    status, body = request("GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    employees = body["employees"]
    return {row["id"]: row for row in employees}, employees


def assert_readable_json_400(testcase, status, headers, raw_body):
    """错误响应本身必须是可按 UTF-8 读取的 JSON，且是 JSON 对象。"""
    testcase.assertEqual(status, 400, f"请求体不成立时必须返回 400，实际 {status}")
    testcase.assertIn(
        "application/json",
        headers.get("Content-Type", ""),
        "错误响应必须声明为 JSON",
    )
    text = raw_body.decode("utf-8")  # 不得抛 UnicodeDecodeError：响应可按 UTF-8 读取
    parsed = json.loads(text)        # 不得抛 JSONDecodeError：响应是合法 JSON
    testcase.assertIsInstance(parsed, dict, "错误响应最外层必须是 JSON 对象")
    return parsed


def assert_format_error(testcase, body, expected_message):
    """请求体格式错误：固定中文原因、不带 field、不混入字段级或重复提示。"""
    testcase.assertEqual(
        body.get("error"),
        expected_message,
        f"错误原因必须是固定中文说明: {body!r}",
    )
    testcase.assertNotIn(
        "field", body, "请求体格式错误不得带 field，不能伪装成某个字段的填写提示"
    )
    message = body.get("error", "")
    for hint in FIELD_ERROR_HINTS:
        testcase.assertNotIn(
            hint, message, f"请求体格式错误不应变成字段/重复提示（含 {hint!r}）: {message!r}"
        )


class UnparseableBodyTest(unittest.TestCase):
    """请求体无法按 UTF-8 读取或无法解析为 JSON：400 + 固定中文原因。"""

    def test_unparseable_bodies_rejected(self):
        for raw, description in UNPARSEABLE_BODIES:
            with self.subTest(description=description):
                status, headers, raw_body = send_raw(raw)
                body = assert_readable_json_400(self, status, headers, raw_body)
                assert_format_error(self, body, UNPARSEABLE_MESSAGE)

    def test_truncated_json_with_valid_fields_not_saved(self):
        """残缺内容里即使已出现合法工号和姓名，也不得据此保存任何档案。"""
        truncated = (
            b'{"employee_no": "J101", "name": "'
            + "张三".encode("utf-8")
            + b'", "department": "'
            + "研发".encode("utf-8")
        )
        by_id_before, rows_before = list_by_id()

        status, headers, raw_body = send_raw(truncated)
        body = assert_readable_json_400(self, status, headers, raw_body)
        assert_format_error(self, body, UNPARSEABLE_MESSAGE)

        by_id_after, rows_after = list_by_id()
        self.assertEqual(by_id_after, by_id_before, "残缺 JSON 提交不得改变员工列表")
        self.assertFalse(
            any(row["employee_no"] == "J101" for row in rows_after),
            "残缺内容中出现的工号 J101 不得被保存",
        )
        self.assertFalse(
            any(row["name"] == "张三" for row in rows_after),
            "残缺内容中出现的姓名“张三”不得被保存",
        )
        self.assertEqual(len(rows_after), len(rows_before))

    def test_invalid_utf8_bytes_rejected_without_partial_save(self):
        """含非法 UTF-8 字节时返回固定格式错误，字节前缀里的工号不得落库。"""
        raw = b'{"employee_no": "J202", "name": "\xe5\xbc\xa0\xff\xe4\xb8\x89"}'
        _, rows_before = list_by_id()

        status, headers, raw_body = send_raw(raw)
        body = assert_readable_json_400(self, status, headers, raw_body)
        assert_format_error(self, body, UNPARSEABLE_MESSAGE)

        _, rows_after = list_by_id()
        self.assertFalse(
            any(row["employee_no"] == "J202" for row in rows_after),
            "无法按 UTF-8 解码的请求不得保存其中出现的工号 J202",
        )
        self.assertEqual(len(rows_after), len(rows_before), "非法字节提交不得新增记录")


class NonObjectTopLevelTest(unittest.TestCase):
    """可解析为 JSON 但最外层不是对象：400 + “必须是 JSON 对象”。"""

    def test_scalar_top_level_values_rejected(self):
        for raw, description in NON_OBJECT_SCALARS:
            with self.subTest(description=description):
                status, headers, raw_body = send_raw(raw)
                body = assert_readable_json_400(self, status, headers, raw_body)
                assert_format_error(self, body, NON_OBJECT_MESSAGE)

    def test_null_body_distinct_from_empty_body(self):
        """字面 null 可以解析但不是对象，原因必须与空请求体不同。"""
        status, headers, raw_body = send_raw(b"null")
        body = assert_readable_json_400(self, status, headers, raw_body)
        assert_format_error(self, body, NON_OBJECT_MESSAGE)
        self.assertNotEqual(body.get("error"), UNPARSEABLE_MESSAGE)

    def test_array_of_complete_employees_never_creates_records(self):
        """数组里装着完整员工对象（一个或多个）也不能被当作建档请求接受。"""
        _, rows_before = list_by_id()
        single = b"[" + _complete_employee_json("J301", "数组甲·单人") + b"]"
        multiple = (
            b"["
            + _complete_employee_json("J302", "数组乙·第一人")
            + b","
            + _complete_employee_json("J303", "数组丙·第二人")
            + b"]"
        )
        for raw, description in ((single, "单元素数组"), (multiple, "两元素数组")):
            with self.subTest(description=description):
                status, headers, raw_body = send_raw(raw)
                body = assert_readable_json_400(self, status, headers, raw_body)
                assert_format_error(self, body, NON_OBJECT_MESSAGE)

        _, rows_after = list_by_id()
        self.assertEqual(
            [row["employee_no"] for row in rows_after],
            [row["employee_no"] for row in rows_before],
            "数组提交绝不能批量或逐个建档",
        )
        for forbidden_no in ("J301", "J302", "J303"):
            self.assertFalse(
                any(row["employee_no"] == forbidden_no for row in rows_after),
                f"数组中的工号 {forbidden_no} 不得被建档",
            )
        for forbidden_name in ("数组甲·单人", "数组乙·第一人", "数组丙·第二人"):
            self.assertFalse(
                any(row["name"] == forbidden_name for row in rows_after),
                f"数组中的姓名 {forbidden_name} 不得被建档",
            )


class MalformedSubmissionPersistenceTest(unittest.TestCase):
    """格式错误提交与档案数据一致：不增不改不覆盖，之后合法建档仍成功。"""

    def test_bad_bodies_leave_existing_records_untouched(self):
        # 已有一条资料完整的基线档案。
        status, baseline_created = request("POST", "/api/employees", BASELINE_EMPLOYEE)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline_created}")
        baseline_id = baseline_created["id"]

        snapshot_before, rows_before = list_by_id()
        self.assertEqual(snapshot_before[baseline_id], baseline_created)
        baseline_no = BASELINE_EMPLOYEE["employee_no"]

        # 每一种坏请求都故意让基线工号 J801 在内容中再次出现：格式判断必须
        # 先于工号判重，结果只能是 400 请求体格式错误，而不能是 409 重复。
        bad_bodies = [
            (
                UNPARSEABLE_MESSAGE,
                b'{"employee_no": "' + baseline_no.encode("utf-8")
                + b'", "name": "' + "撞工号但 JSON 残缺".encode("utf-8")
                + b'", "department":',
                "残缺 JSON 中含已存在工号",
            ),
            (
                UNPARSEABLE_MESSAGE,
                b'{"employee_no":"' + baseline_no.encode("utf-8") + b'","name":"\xff"}',
                "含已存在工号但字节无法按 UTF-8 解码",
            ),
            (
                NON_OBJECT_MESSAGE,
                b"[" + _complete_employee_json(baseline_no, "撞工号但装在数组里") + b"]",
                "数组中的完整对象使用已存在工号",
            ),
            (NON_OBJECT_MESSAGE, b"null", "顶层 null"),
            (NON_OBJECT_MESSAGE, b"12345", "顶层数字"),
            (NON_OBJECT_MESSAGE, b'"' + baseline_no.encode("utf-8") + b'"', "顶层字符串"),
            (NON_OBJECT_MESSAGE, b"true", "顶层布尔"),
            (UNPARSEABLE_MESSAGE, b"", "空请求体"),
            (UNPARSEABLE_MESSAGE, b"   ", "纯空白请求体"),
        ]
        for expected_message, raw, description in bad_bodies:
            with self.subTest(description=description):
                status, headers, raw_body = send_raw(raw)
                self.assertEqual(
                    status, 400,
                    f"{description} 必须返回 400（不能是 409 工号重复）",
                )
                body = assert_readable_json_400(self, status, headers, raw_body)
                assert_format_error(self, body, expected_message)

            # 每次错误提交之后立即核对：列表与提交前完全一致。
            snapshot_now, _ = list_by_id()
            self.assertEqual(
                snapshot_now, snapshot_before,
                f"{description} 之后员工列表必须与提交前完全相同",
            )

        # 已有档案的编号、姓名、部门和联系方式逐项保持原样，没有残缺记录混入。
        snapshot_after, rows_after = list_by_id()
        self.assertEqual(snapshot_after, snapshot_before)
        stored_baseline = snapshot_after[baseline_id]
        self.assertEqual(stored_baseline, baseline_created)
        self.assertEqual(stored_baseline["employee_no"], "J801")
        self.assertEqual(stored_baseline["name"], "基线档案·赵一")
        self.assertEqual(stored_baseline["department"], "人事部")
        self.assertEqual(stored_baseline["position"], "HRBP")
        self.assertEqual(stored_baseline["phone"], "13800000081")
        self.assertEqual(stored_baseline["email"], "base.j801@example.com")
        self.assertEqual(stored_baseline["status"], "在职")
        self.assertFalse(
            any("撞工号" in row["name"] for row in rows_after),
            "坏请求中使用的姓名不得残留在员工列表中",
        )

        # 处理过这些错误之后，下一份资料完整的正常对象必须照常建档。
        status, good_created = request("POST", "/api/employees", AFTER_BAD_BODIES_EMPLOYEE)
        self.assertEqual(status, 201, f"错误提交后合法对象仍应返回 201: {good_created}")
        self.assertNotEqual(good_created["id"], baseline_id)
        self.assertEqual(good_created["status"], "在职")
        self.assertEqual(good_created["employee_no"], "J802")

        final_by_id, final_rows = list_by_id()
        self.assertEqual(set(final_by_id), set(snapshot_before) | {good_created["id"]})
        self.assertEqual(final_by_id[baseline_id], baseline_created, "已有档案必须保持原样")
        self.assertEqual(final_by_id[good_created["id"]], good_created)
        self.assertEqual(
            [row["id"] for row in final_rows], sorted(row["id"] for row in final_rows)
        )


class ObjectBoundaryTest(unittest.TestCase):
    """对象与非对象的边界：{} 进入字段校验；正常中文对象不受新增限制。"""

    def test_empty_object_enters_required_field_validation(self):
        """空对象 {} 是合法对象，应进入必填校验：400 + field=employee_no。"""
        by_id_before, rows_before = list_by_id()

        status, headers, raw_body = send_raw(b"{}")
        body = assert_readable_json_400(self, status, headers, raw_body)
        self.assertEqual(status, 400)
        self.assertEqual(
            body.get("field"), "employee_no",
            "空对象必须先报缺少工号，field=employee_no",
        )
        self.assertIn("工号", body.get("error", ""))
        self.assertIn("不能为空", body.get("error", ""))

        # 与无法解析的内容必须是两种不同的错误原因。
        self.assertNotEqual(body.get("error"), UNPARSEABLE_MESSAGE)
        self.assertNotEqual(body.get("error"), NON_OBJECT_MESSAGE)
        self.assertNotIn("JSON", body.get("error", ""))

        by_id_after, rows_after = list_by_id()
        self.assertEqual(by_id_after, by_id_before, "空对象校验失败不得新增档案")
        self.assertEqual(len(rows_after), len(rows_before))

    def test_chinese_object_saved_and_returned_unchanged(self):
        """含中文姓名/部门、中文联系方式备注和非邮箱字符串的对象正常 201 保存。"""
        status, created = request("POST", "/api/employees", CHINESE_OBJECT_EMPLOYEE)
        self.assertEqual(status, 201, f"中文正常对象应建档成功: {created}")
        self.assertIsInstance(created.get("id"), int)
        self.assertEqual(created["employee_no"], "J901")
        self.assertEqual(created["name"], "陈 明", "姓名内部空格必须保留")
        self.assertEqual(created["department"], "研发 平台部", "中文部门与内部空格原样保存")
        self.assertEqual(created["position"], "后端工程师")
        self.assertEqual(created["effective_date"], "2026-10-04")
        self.assertEqual(
            created["phone"], "分机 9001（中文备注，内部有空格）",
            "不得新增电话号码格式限制，中文备注与内部空格原样保存",
        )
        self.assertEqual(
            created["email"], "不是邮箱·也允许原样保存",
            "不得新增邮箱格式限制，非邮箱字符串原样保存",
        )
        self.assertEqual(created["status"], "在职")

        by_id, rows = list_by_id()
        self.assertIn(created["id"], by_id)
        self.assertEqual(by_id[created["id"]], created, "列表中应能查到保存后的员工")
        self.assertEqual(
            [row["id"] for row in rows], sorted(row["id"] for row in rows)
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
