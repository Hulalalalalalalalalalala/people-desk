#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""请求体本身不成立时的端到端自动化回归测试。

通过真实 HTTP 进程验证公开行为（仅使用标准库）：
1. 请求体为空、JSON 残缺/语法错误、含无法按 UTF-8 解码的字节：
   均返回 400，响应仍是可解析的 JSON，中文说明“请求体不是有效的 JSON 对象”；
2. 残缺内容中即使出现合法工号/姓名，也不得保存其中任何一部分；
3. 内容可解析但最外层是数组、字符串、数字、布尔值或 null：
   返回 400，中文说明“请求体必须是 JSON 对象”；数组中装着完整员工对象同样拒绝；
4. 上述拒绝与档案数据一致：已有员工保持原样，不新增残缺记录，
   错误响应不带 field，不变成工号重复或字段填写提示；
5. 处理这些错误之后，资料完整的正常对象仍可建档（201）并可从列表查询；
6. 空对象 {} 是合法 JSON 对象：进入必填字段校验，返回 400 且 field=employee_no，
   与无法解析的内容使用不同的错误原因；
7. 含中文姓名和部门的正常对象仍正常保存并原样返回。
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

# 基线员工：中文姓名、部门、联系方式齐全，用于验证错误提交前后已有记录保持原样。
BASELINE_EMPLOYEE = {
    "employee_no": "B001",
    "name": "基线员工·陈一",
    "department": "研发部",
    "position": "高级工程师",
    "effective_date": "2026-09-10",
    "phone": "13811110001",
    "email": "baseline.b001@example.com",
}

# 错误提交全部处理完之后，用于验证服务仍可正常建档的员工。
AFTER_ERRORS_EMPLOYEE = {
    "employee_no": "B002",
    "name": "错误之后·王二",
    "department": "产品部",
    "position": "产品经理",
    "effective_date": "2026-10-04",
    "phone": "13811110002",
    "email": "after.errors.b002@example.com",
}

# 无法按 UTF-8 解码的字节：0xFF/0xFE 不是合法的 UTF-8 序列。
INVALID_UTF8_BODY = (
    b'{"employee_no": "B901", "name": "\xff\xfe", "department": "'
    + "研发部".encode("utf-8")
    + b'"}'
)

# 各类“不是有效 JSON 对象”的原始请求体（字节形式，逐条发送）。
# 其中残缺内容刻意带上合法工号 B902 和姓名，验证不会根据部分内容保存档案。
MALFORMED_BODIES = [
    ("空请求体", b""),
    (
        "JSON 内容残缺",
        b'{"employee_no": "B902", "name": "'
        + "残缺提交·郑九".encode("utf-8")
        + b'", "depart',
    ),
    ("JSON 语法错误", b'{"employee_no": "B903", "name": }'),
    ("非 JSON 文本", b"employee_no=B904&name=not-json"),
    ("无法按 UTF-8 解码的字节", INVALID_UTF8_BODY),
]

# 残缺内容中出现的工号/姓名，任何一条都不应进入员工列表。
MALFORMED_LEFTOVERS = ("B901", "B902", "B903", "B904", "残缺提交·郑九")

# 可解析但最外层不是对象的提交：数组、装着完整员工对象的数组、
# 字符串、数字、布尔值、null。
NON_OBJECT_BODIES = [
    ("空数组", b"[]"),
    (
        "装着完整员工对象的数组",
        json.dumps(
            [
                {
                    "employee_no": "B905",
                    "name": "数组包装·冯十",
                    "department": "市场部",
                    "position": "专员",
                    "effective_date": "2026-10-01",
                    "phone": "13811110003",
                    "email": "in.array.b905@example.com",
                }
            ],
            ensure_ascii=False,
        ).encode("utf-8"),
    ),
    ("字符串", b'"just a string"'),
    ("数字", b"42"),
    ("布尔值", b"true"),
    ("null", b"null"),
]

# 非对象提交中出现的工号/姓名，同样不应进入员工列表。
NON_OBJECT_LEFTOVERS = ("B905", "数组包装·冯十")

BASE_URL = None
_proc = None
_tmpdir = None


def _start_server():
    global BASE_URL, _proc, _tmpdir
    _tmpdir = tempfile.mkdtemp(prefix="peopledesk-test-")
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


def raw_request(method, path, data=None, content_type="application/json"):
    """以原始字节发起 HTTP 请求，返回 (status_code, 响应头, 响应字节)。"""
    headers = {}
    if content_type is not None:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(
        BASE_URL + path, data=data, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()


def request(method, path, body=None):
    """发起 JSON HTTP 请求，返回 (status_code, json_body)。"""
    data = None
    if body is not None:
        data = json.dumps(body, ensure_ascii=False).encode("utf-8")
    status, _headers, raw = raw_request(method, path, data)
    return status, json.loads(raw.decode("utf-8"))


def list_employees():
    status, body = request("GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    return body["employees"]


class InvalidRequestBodyTest(unittest.TestCase):
    def assert_json_error(self, status, headers, raw, expected_status):
        """错误响应应是可读取的 JSON，且 Content-Type 标明 JSON。"""
        self.assertEqual(status, expected_status, f"应返回 {expected_status}: {raw!r}")
        content_type = headers.get("Content-Type", "")
        self.assertIn("application/json", content_type)
        try:
            body = json.loads(raw.decode("utf-8"))
        except (ValueError, UnicodeDecodeError) as exc:
            self.fail(f"错误响应应是可读取的 JSON: {exc}: {raw!r}")
        self.assertIsInstance(body, dict)
        return body

    def assert_body_format_error(self, body):
        """错误应明确来自请求体格式判断：不带 field，不含字段级或工号重复提示。"""
        message = body.get("error", "")
        self.assertIsInstance(message, str)
        self.assertTrue(message, "错误响应应包含中文原因")
        self.assertNotIn("field", body, "请求体格式错误不应带 field 字段")
        self.assertNotIn("已存在", message, "不应变成工号重复提示")
        self.assertNotIn("不能为空", message, "不应变成某个员工字段的填写提示")

    def test_malformed_and_non_object_bodies(self):
        """基线建档 → 各类无效请求体被拒 → 列表保持原样 → 之后仍可正常建档。"""

        # 0. 记录初始列表（同模块其他测试可能已建档），其中不应已有 B001。
        initial_rows = list_employees()
        self.assertFalse(
            any(row["employee_no"] == BASELINE_EMPLOYEE["employee_no"] for row in initial_rows),
            "初始列表中不应已有基线工号，确保可重复执行",
        )

        # 1. 基线员工：中文姓名/部门正常建档，响应原样返回。
        status, baseline = request("POST", "/api/employees", BASELINE_EMPLOYEE)
        self.assertEqual(status, 201, f"基线建档应成功: {baseline}")
        self.assertIsInstance(baseline.get("id"), int)
        self.assertEqual(baseline["employee_no"], BASELINE_EMPLOYEE["employee_no"])
        self.assertEqual(baseline["name"], BASELINE_EMPLOYEE["name"])
        self.assertEqual(baseline["department"], BASELINE_EMPLOYEE["department"])
        self.assertEqual(baseline["position"], BASELINE_EMPLOYEE["position"])
        self.assertEqual(
            baseline["effective_date"], BASELINE_EMPLOYEE["effective_date"]
        )
        self.assertEqual(baseline["phone"], BASELINE_EMPLOYEE["phone"])
        self.assertEqual(baseline["email"], BASELINE_EMPLOYEE["email"])
        self.assertEqual(baseline["status"], "在职")

        snapshot = list_employees()
        self.assertEqual(snapshot, initial_rows + [baseline])

        # 2. 空请求体、残缺/错误 JSON、非 UTF-8 字节：400 + “不是有效的 JSON 对象”。
        for label, raw_body in MALFORMED_BODIES:
            with self.subTest(无效请求体=label):
                status, headers, raw = raw_request("POST", "/api/employees", raw_body)
                body = self.assert_json_error(status, headers, raw, 400)
                self.assertIn("不是有效的 JSON 对象", body.get("error", ""))
                self.assert_body_format_error(body)
                self.assertEqual(
                    list_employees(), snapshot, f"{label} 不得改变员工列表"
                )

        # 3. 可解析但最外层不是对象：400 + “必须是 JSON 对象”。
        for label, raw_body in NON_OBJECT_BODIES:
            with self.subTest(非对象请求体=label):
                status, headers, raw = raw_request("POST", "/api/employees", raw_body)
                body = self.assert_json_error(status, headers, raw, 400)
                self.assertIn("必须是 JSON 对象", body.get("error", ""))
                self.assertNotIn("不是有效", body.get("error", ""))
                self.assert_body_format_error(body)
                self.assertEqual(
                    list_employees(), snapshot, f"{label} 不得改变员工列表"
                )

        # 4. 残缺内容或数组中出现过的工号/姓名，不得留下任何记录。
        rows = list_employees()
        for leftover in MALFORMED_LEFTOVERS + NON_OBJECT_LEFTOVERS:
            self.assertFalse(
                any(
                    leftover in (row.get("employee_no") or "")
                    or leftover in (row.get("name") or "")
                    for row in rows
                ),
                f"被拒绝提交中的 {leftover!r} 不得出现在员工列表中",
            )

        # 5. 空对象 {} 是合法 JSON 对象：进入必填字段校验，field=employee_no，
        #    且错误原因与无法解析的内容不同。
        status, headers, raw = raw_request("POST", "/api/employees", b"{}")
        body = self.assert_json_error(status, headers, raw, 400)
        self.assertEqual(body.get("field"), "employee_no")
        self.assertIn("工号", body.get("error", ""))
        self.assertNotIn("JSON 对象", body.get("error", ""))
        self.assertEqual(list_employees(), snapshot, "空对象校验失败不得新增档案")

        # 6. 处理完这些错误之后，资料完整的正常对象仍可建档并查询到。
        status, created = request("POST", "/api/employees", AFTER_ERRORS_EMPLOYEE)
        self.assertEqual(status, 201, f"错误处理后正常对象仍应建档成功: {created}")
        self.assertEqual(created["employee_no"], AFTER_ERRORS_EMPLOYEE["employee_no"])
        self.assertEqual(created["name"], AFTER_ERRORS_EMPLOYEE["name"])
        self.assertEqual(created["department"], AFTER_ERRORS_EMPLOYEE["department"])
        self.assertEqual(created["status"], "在职")

        final_rows = list_employees()
        self.assertEqual(len(final_rows), len(initial_rows) + 2)
        by_id = {row["id"]: row for row in final_rows}
        self.assertEqual(
            by_id[baseline["id"]], baseline, "已有员工的全部内容应保持原样"
        )
        self.assertEqual(by_id[created["id"]], created)
        for row in initial_rows:
            self.assertEqual(
                by_id[row["id"]], row, "测试开始前已有的员工记录不得被改动"
            )

    def test_chinese_content_still_saved_verbatim(self):
        """含中文姓名和部门的正常对象仍正常保存并原样返回，不新增内容限制。"""
        payload = {
            "employee_no": "B003",
            "name": "中文姓名·林三",
            "department": "质量保障部",
            "position": "测试工程师",
            "effective_date": "2026-12-31",
            "phone": "138-1111-0003",
            "email": "中文邮箱前缀.b003@example.com",
        }
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"中文内容建档应成功: {created}")
        for key, value in payload.items():
            self.assertEqual(created[key], value, f"{key} 应原样保存")

        rows = list_employees()
        matched = [row for row in rows if row["id"] == created["id"]]
        self.assertEqual(matched, [created], "列表中应能查询到保存后的员工")


if __name__ == "__main__":
    unittest.main(verbosity=2)
