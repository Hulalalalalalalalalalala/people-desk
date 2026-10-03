#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""任职生效日期的端到端自动化回归测试。

通过真实 HTTP 进程验证公开行为（仅使用标准库），沿用建档与列表查询接口：
1. effective_date 为 YYYY-MM-DD 形式的真实日历日期时返回 201，响应与随后
   的员工列表查询中日期均与提交值一致，其余档案内容按原有规则保存；
2. 普通日期、闰年 2 月 29 日、未来日期均允许建档，未来日期不影响“在职”状态，
   服务不得把日期改成提交当天，也不得要求等到生效日才允许保存；
3. 格式不符或格式正确但日历中不存在该日（含闰年规则、每月实际天数、
   少写位数、附带时间、两侧空白）一律 400，中文原因并以 field=effective_date
   指向生效日期，不能被自动修正后接受；
4. 未提供日期或传入非字符串时保持现有的字段错误响应（400 + field），
   不能变成服务异常或成功建档；
5. 日期是提交中唯一问题时，必须得到日期错误而非工号冲突；失败提交不新增、
   不覆盖，不留下日期为空的残缺档案，已有员工在列表中保持原样；
6. 工号唯一性规则及其回归保障继续成立（本模块末尾保留一条 409 回归）。
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

# 与工号唯一性测试使用不同号段（A0xx/b010），同一数据目录内互不干扰。
VALID_REGULAR_NO = "D101"
VALID_LEAP_NO = "D102"
VALID_CENTURY_LEAP_NO = "D103"
VALID_FUTURE_NO = "D104"
DUPLICATE_NO = "D105"
BASELINE_NO = "D300"


def make_payload(employee_no, effective_date, *, name=None):
    """构造一份除工号与日期外均符合录入要求的建档请求。"""
    return {
        "employee_no": employee_no,
        "name": name or f"日期测试·{employee_no}",
        "department": " 研发部 ",
        "position": " 后端工程师 ",
        "effective_date": effective_date,
        "phone": f"138{employee_no[-3:]}0000",
        "email": f"{employee_no.lower()}@example.com",
    }


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


def list_by_id():
    status, body = request("GET", "/api/employees")
    assert status == 200, f"列表查询失败: {status} {body}"
    employees = body["employees"]
    return {row["id"]: row for row in employees}, employees


def assert_date_error(testcase, status, body):
    """非法日期必须是 400 + 中文原因 + field=effective_date，而非服务异常或工号冲突。"""
    testcase.assertEqual(status, 400, f"非法日期应返回 400: {body}")
    testcase.assertEqual(body.get("field"), "effective_date")
    message = body.get("error", "")
    testcase.assertIn("日期", message)
    # 明确的日期错误，不能误报成工号冲突。
    testcase.assertNotEqual(body.get("field"), "employee_no")
    testcase.assertNotIn("工号已存在", message)


def assert_saved_as_submitted(testcase, payload, submitted_date, created):
    """创建响应中的日期与其余字段均按提交值/既有规则保存。"""
    testcase.assertEqual(created["effective_date"], submitted_date)
    testcase.assertEqual(created["status"], "在职")
    testcase.assertEqual(created["employee_no"], payload["employee_no"].strip())
    testcase.assertEqual(created["name"], payload["name"].strip())
    testcase.assertEqual(created["department"], payload["department"].strip())
    testcase.assertEqual(created["position"], payload["position"].strip())
    testcase.assertEqual(created["phone"], payload["phone"].strip())
    testcase.assertEqual(created["email"], payload["email"].strip())


class ValidEffectiveDateTest(unittest.TestCase):
    def test_regular_date_is_saved_and_queryable(self):
        """普通合法日期：201 创建，响应与列表查询中的日期均与提交值一致。"""
        payload = make_payload(VALID_REGULAR_NO, "2026-09-01")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"普通合法日期应建档成功: {created}")
        assert_saved_as_submitted(self, payload, "2026-09-01", created)

        by_id, _ = list_by_id()
        self.assertIn(created["id"], by_id)
        self.assertEqual(by_id[created["id"]], created, "列表查询结果应与创建响应一致")
        self.assertEqual(by_id[created["id"]]["effective_date"], "2026-09-01")

    def test_leap_day_feb_29_accepted(self):
        """闰年 2 月 29 日允许录入：2024-02-29（闰年）必须成功。"""
        payload = make_payload(VALID_LEAP_NO, "2024-02-29", name="闰日建档·D102")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"闰年 2024-02-29 应建档成功: {created}")
        assert_saved_as_submitted(self, payload, "2024-02-29", created)

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]]["effective_date"], "2024-02-29")

    def test_year_2000_leap_day_accepted(self):
        """能被 400 整除的世纪年 2000 年也是闰年：2000-02-29 必须成功。"""
        payload = make_payload(VALID_CENTURY_LEAP_NO, "2000-02-29")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"2000-02-29 是真实日历日期，应建档成功: {created}")
        self.assertEqual(created["effective_date"], "2000-02-29")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]]["effective_date"], "2000-02-29")

    def test_future_date_accepted_with_active_status(self):
        """未来日期可以建档：日期原样保存、状态仍为“在职”，不改成提交当天。"""
        future_date = "2099-12-31"
        payload = make_payload(VALID_FUTURE_NO, future_date, name="未来生效·D104")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"未来日期应允许建档: {created}")
        self.assertEqual(created["effective_date"], future_date,
                         "未来日期必须原样保存，不能被改成提交当天")
        self.assertEqual(created["status"], "在职", "未来日期建档时任职状态仍应为“在职”")

        by_id, _ = list_by_id()
        stored = by_id[created["id"]]
        self.assertEqual(stored, created)
        self.assertEqual(stored["effective_date"], future_date)
        self.assertEqual(stored["status"], "在职")


class MalformedEffectiveDateTest(unittest.TestCase):
    """格式不符：少写位数、分隔符错误、附带时间、两侧空白、空串等一律拒绝。"""

    MALFORMED_VALUES = [
        ("2026-9-01", "月份少写一位"),
        ("2026-09-1", "日期少写一位"),
        ("26-09-01", "年份少写位数"),
        ("2026/09/01", "错误分隔符"),
        ("20260901", "没有分隔符"),
        ("2024-02-29 08:00", "附带日期后时间与空格"),
        ("2024-02-29T08:00:00", "附带 ISO 时间部分"),
        (" 2024-02-29", "日期前带空白"),
        ("2024-02-29 ", "日期后带空白"),
        ("\t2024-02-29\n", "日期两侧带制表符/换行"),
        ("", "空字符串"),
        ("二〇二六-〇九-〇一", "非数字字符"),
    ]

    def test_malformed_values_rejected(self):
        for index, (value, description) in enumerate(self.MALFORMED_VALUES):
            with self.subTest(value=value, description=description):
                # 每次使用全新工号，保证日期是提交中唯一的问题。
                employee_no = f"D2{index:02d}"
                payload = make_payload(employee_no, value)
                status, body = request("POST", "/api/employees", payload)
                assert_date_error(self, status, body)

    def test_missing_or_non_string_date_rejected(self):
        """未提供日期或传入非字符串：保持现有的字段错误响应，不变成服务异常/成功建档。"""
        bad_bodies = [
            (None, "缺少 effective_date 字段"),
            (None, "日期为 null"),
            (20260901, "日期为数字"),
            (["2026-09-01"], "日期为数组"),
            (True, "日期为布尔值"),
            ({"year": 2026}, "日期为对象"),
        ]
        for index, (value, description) in enumerate(bad_bodies):
            with self.subTest(description=description):
                # 每次使用全新工号，保证日期是提交中唯一的问题。
                employee_no = f"D25{index}"
                payload = make_payload(employee_no, "2026-09-01")
                if index == 0:
                    del payload["effective_date"]  # 真正不提供该字段
                elif index == 1:
                    payload["effective_date"] = None
                else:
                    payload["effective_date"] = value
                status, body = request("POST", "/api/employees", payload)
                assert_date_error(self, status, body)


class NonexistentCalendarDateTest(unittest.TestCase):
    """格式正确但日历中没有这一天：闰年规则与每月实际天数都必须保障。"""

    NONEXISTENT_VALUES = [
        ("2026-02-29", "2026 年不是闰年，2 月没有 29 日"),
        ("2026-04-31", "4 月只有 30 天"),
        ("1900-02-29", "1900 年是不能被 400 整除的世纪年，不是闰年"),
        ("2026-13-01", "月份超出 12"),
        ("2026-00-15", "月份为 00"),
        ("2026-09-00", "日期为 00"),
        ("2026-09-32", "日期超过当月天数"),
        ("2026-11-31", "11 月只有 30 天"),
    ]

    def test_nonexistent_dates_rejected(self):
        for index, (value, description) in enumerate(self.NONEXISTENT_VALUES):
            with self.subTest(value=value, description=description):
                employee_no = f"D27{index}"
                payload = make_payload(employee_no, value)
                status, body = request("POST", "/api/employees", payload)
                assert_date_error(self, status, body)


class RejectionPersistenceTest(unittest.TestCase):
    def test_failed_date_submission_leaves_no_trace(self):
        """非法日期拒绝后：已有员工保持原样，失败工号缺席，无日期为空的残缺档案。"""
        baseline = make_payload(BASELINE_NO, "2024-02-29", name="基线员工·D300")
        status, created = request("POST", "/api/employees", baseline)
        self.assertEqual(status, 201, f"基线档案应建档成功: {created}")

        snapshot_before, rows_before = list_by_id()
        self.assertIn(created["id"], snapshot_before)

        # 全新工号 + 除日期外全部合法：日期是唯一问题，必须报日期错误而非工号冲突。
        for failed_no, bad_date in (
            ("D301", "2026-02-29"),       # 非闰年闰日
            ("D302", "2026-04-31"),       # 当月无此日
            ("D303", "2026-9-1"),         # 位数不足
            ("D304", "2024-02-29T08:00"),  # 附带时间
            (" D305 ", "2024-02-29 "),    # 日期两侧带空白（工号本身合法）
        ):
            payload = make_payload(failed_no, bad_date, name=f"失败提交·{failed_no.strip()}")
            status, body = request("POST", "/api/employees", payload)
            assert_date_error(self, status, body)

        snapshot_after, rows_after = list_by_id()

        # 已有员工（含基线档案及其日期）逐字段保持原样。
        self.assertEqual(snapshot_after, snapshot_before,
                         "非法日期提交被拒绝后，已有员工档案必须保持原样")
        self.assertEqual(snapshot_after[created["id"]], created)
        self.assertEqual(snapshot_after[created["id"]]["effective_date"], "2024-02-29")

        # 失败提交使用的工号一律不得出现在列表中（去掉首尾空白后也查不到）。
        failed_numbers = {"d301", "d302", "d303", "d304", "d305"}
        present_numbers = {row["employee_no"].strip().lower() for row in rows_after}
        self.assertTrue(
            failed_numbers.isdisjoint(present_numbers),
            f"失败工号不得出现在员工列表中: {sorted(failed_numbers & present_numbers)}",
        )

        # 不允许留下日期为空/None 的残缺档案。
        self.assertFalse(
            any(not row.get("effective_date") for row in rows_after),
            "失败提交不得留下日期为空的残缺档案",
        )

        # 失败提交使用的姓名同样不得残留。
        self.assertFalse(
            any(row["name"].startswith("失败提交·") for row in rows_after),
            "失败提交使用的姓名不得出现在员工列表中",
        )

    def test_today_is_not_silently_substituted(self):
        """非法日期不能被静默修正；失败后以同一工号提交合法日期应正常成功。"""
        employee_no = "D306"
        bad_payload = make_payload(employee_no, "2026-02-29")
        status, body = request("POST", "/api/employees", bad_payload)
        assert_date_error(self, status, body)

        by_id, _ = list_by_id()
        self.assertFalse(
            any(row["employee_no"] == employee_no for row in by_id.values()),
            "被拒绝的提交不得提前占用工号",
        )

        good_payload = make_payload(employee_no, "2028-02-29", name="改正后建档·D306")
        status, created = request("POST", "/api/employees", good_payload)
        self.assertEqual(status, 201, f"改正为合法闰日后应建档成功: {created}")
        self.assertEqual(created["effective_date"], "2028-02-29")
        self.assertEqual(created["status"], "在职")


class EmployeeNoUniquenessRegressionTest(unittest.TestCase):
    """日期保障不得削弱工号唯一性规则：同工号重复建档仍返回 409。"""

    def test_duplicate_employee_no_still_conflicts(self):
        payload = make_payload(DUPLICATE_NO, "2024-02-29", name="唯一性基线·D105")
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"首次建档应成功: {created}")

        snapshot_before, _ = list_by_id()

        duplicate = make_payload(DUPLICATE_NO.lower(), "2026-10-03",
                                 name="重复工号·不应保存")
        status, body = request("POST", "/api/employees", duplicate)
        self.assertEqual(status, 409, f"重复工号应返回 409: {body}")
        self.assertEqual(body.get("field"), "employee_no")
        self.assertIn("工号", body.get("error", ""))
        self.assertIn("已存在", body.get("error", ""))

        snapshot_after, rows_after = list_by_id()
        self.assertEqual(snapshot_after, snapshot_before, "重复工号提交不得新增或覆盖档案")
        self.assertEqual(snapshot_after[created["id"]], created)
        self.assertFalse(
            any(row["name"] == "重复工号·不应保存" for row in rows_after),
            "重复提交的档案不得出现在员工列表中",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
