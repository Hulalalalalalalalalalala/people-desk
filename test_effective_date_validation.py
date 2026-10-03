#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""任职生效日期（effective_date）格式与日历边界的端到端自动化回归测试。

通过真实 HTTP 进程验证公开行为（仅使用标准库）：
1. YYYY-MM-DD 形式的真实日历日期（普通日期、闰年 2 月 29 日、未来日期）
   均可建档，返回 201，日期原样保存，任职状态为“在职”，列表查询结果一致；
2. 格式不符（位数不足、附带时间、两侧空白）或日历中不存在该天
   （非闰年 2 月 29 日、4 月 31 日等）返回 400，中文原因，field=effective_date；
3. 未提供日期或传入非字符串保持现有字段错误响应（400，field=effective_date），
   不变成服务异常或成功建档；
4. 日期是提交中的唯一问题时返回明确的日期错误而非工号冲突；
5. 被拒绝的提交不新增档案、不留下日期为空的残缺档案，已有员工保持原样。

工号唯一性规则及其回归保障见 test_employee_no_uniqueness.py，本文件不复述。
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


def make_payload(employee_no, effective_date, name):
    """构造一份除工号/日期/姓名外均符合录入要求的档案。"""
    return {
        "employee_no": employee_no,
        "name": name,
        "department": "研发部",
        "position": "工程师",
        "effective_date": effective_date,
        "phone": "13800000010",
        "email": f"{employee_no.lower()}@example.com",
    }


# 合法日期：普通日期、闰年 2 月 29 日、未来日期（相对提交当天仍在未来）。
VALID_CASES = [
    ("D001", "2026-10-02", "普通日期·赵一"),
    ("D002", "2024-02-29", "闰日建档·钱二"),
    ("D003", "2031-12-31", "未来日期·孙三"),
]

# 日历中不存在该天：闰年规则与每月实际天数都必须真正校验。
INVALID_CALENDAR_DATES = [
    "2026-02-29",  # 2026 年不是闰年，2 月没有 29 日
    "2026-04-31",  # 4 月只有 30 天
    "2026-02-30",  # 任何年份 2 月都没有 30 日
    "2026-13-01",  # 没有第 13 个月
    "2026-00-10",  # 没有第 0 个月
    "2026-01-00",  # 没有第 0 日
    "2026-01-32",  # 1 月只有 31 天
]

# 格式不符：不能像日期就被接受，也不能自动修正。
INVALID_FORMAT_DATES = [
    "2026-1-01",    # 月份少一位
    "2026-01-1",    # 日期少一位
    "2026-1-1",     # 月份日期都少位
    "26-01-01",     # 年份少位
    "2026-10-02T00:00:00",  # 附带时间
    "2026-10-02 08:30",     # 附带时间（空格分隔）
    " 2026-10-02",  # 左侧空白
    "2026-10-02 ",  # 右侧空白
    " 2026-10-02 ", # 两侧空白
    "2026/10/02",   # 错误的分隔符
    "2026年10月02日",
    "not-a-date",
    "",
]

# 未提供或类型不符：保持现有字段错误响应。
MISSING_KEY = object()
NON_STRING_DATES = [
    MISSING_KEY,   # 请求中不带 effective_date
    None,
    20261002,
    2026.1002,
    True,
    ["2026-10-02"],
    {"date": "2026-10-02"},
]

BASE_URL = None
_proc = None
_tmpdir = None


def _start_server():
    global BASE_URL, _proc, _tmpdir
    _tmpdir = tempfile.mkdtemp(prefix="peopledesk-date-test-")
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


def list_employees():
    status, body = request("GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    return body["employees"]


def assert_effective_date_error(testcase, status, body):
    """日期问题应返回 400，中文原因，field 指向 effective_date。"""
    testcase.assertEqual(status, 400, f"无效日期应返回 400: {body}")
    testcase.assertEqual(body.get("field"), "effective_date")
    message = body.get("error", "")
    testcase.assertTrue(message, "错误响应必须给出中文原因")
    testcase.assertIn("日期", message)


class EffectiveDateValidTest(unittest.TestCase):
    """合法日期：普通日期、闰年 2 月 29 日、未来日期都应成功建档。"""

    def test_valid_dates_are_saved_and_listed(self):
        before_nos = {row["employee_no"] for row in list_employees()}
        self.assertFalse(
            before_nos & {no for no, _, _ in VALID_CASES},
            "测试数据目录中不应已有本用例工号，确保可重复执行",
        )

        created_by_no = {}
        for employee_no, date, name in VALID_CASES:
            with self.subTest(effective_date=date):
                payload = make_payload(employee_no, date, name)
                status, created = request("POST", "/api/employees", payload)
                self.assertEqual(status, 201, f"合法日期 {date} 应建档成功: {created}")
                self.assertIsInstance(created.get("id"), int)
                # 创建结果中的日期与提交值一致，不被改成提交当天或其他值。
                self.assertEqual(created["effective_date"], date)
                self.assertEqual(created["employee_no"], employee_no)
                self.assertEqual(created["name"], name)
                self.assertEqual(created["department"], payload["department"])
                self.assertEqual(created["position"], payload["position"])
                self.assertEqual(created["phone"], payload["phone"])
                self.assertEqual(created["email"], payload["email"])
                # 未来日期同样立即建档，任职状态仍为“在职”。
                self.assertEqual(created["status"], "在职")
                created_by_no[employee_no] = created

        # 查询员工列表应得到同一批员工及同一日期，内容与创建响应逐字段一致。
        listed_by_no = {row["employee_no"]: row for row in list_employees()}
        for employee_no, created in created_by_no.items():
            self.assertIn(employee_no, listed_by_no)
            self.assertEqual(listed_by_no[employee_no], created)
            self.assertEqual(listed_by_no[employee_no]["status"], "在职")

    def test_future_date_not_rewritten_to_today(self):
        """未来日期必须原样保存，不能被改成提交当天。"""
        payload = make_payload("D004", "2030-06-15", "未来不改写·李四")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"未来日期应建档成功: {created}")
        self.assertEqual(created["effective_date"], "2030-06-15")
        self.assertEqual(created["status"], "在职")

        listed = {row["employee_no"]: row for row in list_employees()}
        self.assertEqual(listed["D004"]["effective_date"], "2030-06-15")


class EffectiveDateInvalidTest(unittest.TestCase):
    """非法日期：400 + 中文原因 + field=effective_date，且不留下任何档案。"""

    def _assert_rejected(self, payload, bad_date_desc):
        before = list_employees()
        status, body = request("POST", "/api/employees", payload)
        assert_effective_date_error(self, status, body)

        after = list_employees()
        self.assertEqual(after, before, f"拒绝 {bad_date_desc} 后列表不得变化")
        self.assertFalse(
            any(row["employee_no"] == payload["employee_no"] for row in after),
            f"失败提交的工号 {payload['employee_no']} 不得出现在列表中",
        )
        self.assertFalse(
            any(row["name"] == payload["name"] for row in after),
            "失败提交不得留下残缺档案",
        )

    def test_nonexistent_calendar_dates_rejected(self):
        for index, bad_date in enumerate(INVALID_CALENDAR_DATES):
            with self.subTest(effective_date=bad_date):
                employee_no = f"E{index:03d}"
                payload = make_payload(employee_no, bad_date, f"日历无效·{bad_date}")
                self._assert_rejected(payload, bad_date)

    def test_malformed_dates_rejected(self):
        for index, bad_date in enumerate(INVALID_FORMAT_DATES):
            with self.subTest(effective_date=bad_date):
                employee_no = f"F{index:03d}"
                payload = make_payload(employee_no, bad_date, f"格式无效·{index}")
                self._assert_rejected(payload, bad_date)

    def test_missing_or_non_string_date_rejected(self):
        for index, bad_value in enumerate(NON_STRING_DATES):
            with self.subTest(effective_date=repr(bad_value)):
                employee_no = f"G{index:03d}"
                payload = make_payload(employee_no, "2026-10-02", f"类型无效·{index}")
                if bad_value is MISSING_KEY:
                    del payload["effective_date"]
                else:
                    payload["effective_date"] = bad_value
                self._assert_rejected(payload, repr(bad_value))

    def test_date_error_reported_when_it_is_the_only_problem(self):
        """工号未被使用、其余资料合规时，唯一问题是日期就必须返回日期错误。"""
        payload = make_payload("E900", "2026-02-29", "唯一问题·周五")
        before = list_employees()
        self.assertFalse(
            any(row["employee_no"] == "E900" for row in before),
            "前置条件：工号 E900 未被使用",
        )
        status, body = request("POST", "/api/employees", payload)
        assert_effective_date_error(self, status, body)
        self.assertNotEqual(status, 409, "日期是唯一问题时不应报工号冲突")

    def test_no_partial_record_with_empty_date(self):
        """列表中的任何员工都不得带有空白或格式残缺的生效日期。"""
        for row in list_employees():
            self.assertTrue(
                row["effective_date"],
                f"员工 {row['employee_no']} 不得保存为空白日期",
            )
            self.assertRegex(row["effective_date"], r"^\d{4}-\d{2}-\d{2}$")


if __name__ == "__main__":
    unittest.main(verbosity=2)
