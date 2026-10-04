#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""必填/选填文字字段校验的端到端自动化回归测试。

通过真实 HTTP 进程验证公开行为（仅使用标准库），沿用 POST /api/employees
与员工列表查询接口，不依赖首页表单的必填提示：

1. 工号、姓名、部门、岗位为合法字符串时返回 201：去掉首尾空白后保存，
   创建响应与随后的列表查询内容一致；文字内部的空格、大小写和中文不被改写
   （如姓名“  陈  明  ”保存为“陈  明”，岗位内部空格保留）；任职状态为
   “在职”，员工编号由服务生成。
2. 必填文字字段缺失、为空字符串或只有空白时返回 400，中文说明对应资料
   不能为空，field 指向实际出错的字段（姓名、部门、岗位各自保障，
   不只依赖已有的空白工号案例）。
3. 必填文字字段传入 null、数字、布尔值、数组或对象时返回 400，中文说明
   该字段必须是字符串，不得转换成文字后建档，field 同样指向出错字段。
4. 电话、邮箱选填：省略、空字符串或只有空白时允许建档并保存为空字符串；
   有文字时只去掉两端空白，不做号码/邮箱格式限制；显式传入非字符串
   （含 null）返回 400，中文原因与 field 对应电话或邮箱。
5. 任何校验失败的提交都不新增残缺记录，已有档案的内容与编号保持原样，
   失败之后合法提交仍可正常建档。
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

# 本模块使用 T 开头号段，与其他回归测试（A0xx/b010/D 系列）互不干扰。
FIELD_LABELS = {
    "employee_no": "工号",
    "name": "姓名",
    "department": "部门",
    "position": "岗位",
}
TEXT_FIELDS = ("employee_no", "name", "department", "position")


def make_payload(employee_no, **overrides):
    """构造一份全部符合录入要求的建档请求，可按需覆盖单个字段。"""
    payload = {
        "employee_no": employee_no,
        "name": f"文字测试·{employee_no}",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-09-01",
        "phone": "13800001234",
        "email": f"{employee_no.lower()}@example.com",
    }
    payload.update(overrides)
    return payload


BASE_URL = None
_proc = None
_tmpdir = None


def _start_server():
    global BASE_URL, _proc, _tmpdir
    _tmpdir = tempfile.mkdtemp(prefix="peopledesk-text-test-")
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


def assert_empty_rejected(testcase, status, body, field):
    """缺失/空串/纯空白：400 + 中文“不能为空”+ field 指向出错字段。"""
    testcase.assertEqual(status, 400, f"{field} 为空应返回 400: {body}")
    testcase.assertEqual(body.get("field"), field,
                         f"field 应指向实际出错的 {field}: {body}")
    message = body.get("error", "")
    testcase.assertIn(FIELD_LABELS[field], message)
    testcase.assertIn("不能为空", message)


def assert_type_rejected(testcase, status, body, field):
    """非字符串类型：400 + 中文“必须是字符串”+ field 指向出错字段。"""
    testcase.assertEqual(status, 400, f"{field} 为非字符串应返回 400: {body}")
    testcase.assertEqual(body.get("field"), field,
                         f"field 应指向实际出错的 {field}: {body}")
    message = body.get("error", "")
    testcase.assertIn(FIELD_LABELS[field], message)
    testcase.assertIn("必须是字符串", message)


class ValidTextFieldsTest(unittest.TestCase):
    """合法文字：去首尾空白后保存，内部空格/大小写/中文不被改写。"""

    def test_trimmed_text_saved_and_queryable(self):
        """姓名“  陈  明  ”保存为“陈  明”，创建响应与列表查询一致。"""
        payload = make_payload(
            "T001",
            name="  陈  明  ",
            department="  研发部  ",
            position="  后端工程师  ",
        )
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"合法文字应建档成功: {created}")
        self.assertIsInstance(created.get("id"), int, "员工编号应由服务生成")
        self.assertEqual(created["employee_no"], "T001")
        self.assertEqual(created["name"], "陈  明",
                         "只去掉首尾空白，姓名内部的空格必须保留")
        self.assertEqual(created["department"], "研发部")
        self.assertEqual(created["position"], "后端工程师")
        self.assertEqual(created["status"], "在职")

        by_id, _ = list_by_id()
        self.assertIn(created["id"], by_id)
        self.assertEqual(by_id[created["id"]], created,
                         "列表查询到的档案应与创建响应逐字段一致")
        self.assertEqual(by_id[created["id"]]["name"], "陈  明")

    def test_inner_space_case_and_chinese_preserved(self):
        """岗位内部空格、英文大小写与中文内容均原样保存，不被顺便改写。"""
        payload = make_payload(
            " T002 ",
            name=" Alice 张 San ",
            department=" R&D 研发部 ",
            position=" 高级  后端  工程师 ",
        )
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"合法文字应建档成功: {created}")
        self.assertEqual(created["employee_no"], "T002")
        self.assertEqual(created["name"], "Alice 张 San",
                         "大小写与内部空格不得被统一改写")
        self.assertEqual(created["department"], "R&D 研发部")
        self.assertEqual(created["position"], "高级  后端  工程师",
                         "岗位中的内部空格必须保留")
        self.assertEqual(created["status"], "在职")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]], created)


class RequiredTextEmptyTest(unittest.TestCase):
    """必填文字字段缺失、空字符串或只有空白：400 + 中文原因 + field。"""

    EMPTY_VARIANTS = [
        ("missing", "未提供该字段"),
        ("", "空字符串"),
        ("   ", "只有空格"),
        (" \t\n ", "只有空白字符"),
    ]

    def test_each_required_field_empty_rejected(self):
        for field in TEXT_FIELDS:
            for index, (value, description) in enumerate(self.EMPTY_VARIANTS):
                with self.subTest(field=field, description=description):
                    # 每次使用全新工号，保证被测字段是提交中唯一的问题。
                    employee_no = f"T1{TEXT_FIELDS.index(field)}{index}"
                    payload = make_payload(employee_no)
                    if value == "missing":
                        del payload[field]
                    else:
                        payload[field] = value
                    status, body = request("POST", "/api/employees", payload)
                    assert_empty_rejected(self, status, body, field)

                    by_id, rows = list_by_id()
                    self.assertFalse(
                        any(row["employee_no"] == employee_no for row in rows),
                        f"{field} {description}的失败提交不得新增档案",
                    )


class RequiredTextTypeTest(unittest.TestCase):
    """必填文字字段传入非字符串：400，说明必须是字符串，不得转换后建档。"""

    NON_STRING_VALUES = [
        (None, "null"),
        (123, "数字"),
        (True, "布尔值"),
        (["张三"], "数组"),
        ({"value": "张三"}, "对象"),
    ]

    def test_each_required_field_non_string_rejected(self):
        for field in TEXT_FIELDS:
            for index, (value, description) in enumerate(self.NON_STRING_VALUES):
                with self.subTest(field=field, description=description):
                    employee_no = f"T2{TEXT_FIELDS.index(field)}{index}"
                    payload = make_payload(employee_no)
                    payload[field] = value
                    status, body = request("POST", "/api/employees", payload)
                    assert_type_rejected(self, status, body, field)

                    _, rows = list_by_id()
                    self.assertFalse(
                        any(row["employee_no"] == employee_no for row in rows),
                        f"{field} 为{description}的失败提交不得新增档案",
                    )


class OptionalContactTest(unittest.TestCase):
    """电话、邮箱选填：省略/空串/纯空白保存为空字符串；有文字只去两端空白。"""

    def test_omitted_contact_saved_as_empty_string(self):
        payload = make_payload("T301")
        del payload["phone"]
        del payload["email"]
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"省略联系方式应允许建档: {created}")
        self.assertEqual(created["phone"], "")
        self.assertEqual(created["email"], "")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]], created)
        self.assertEqual(by_id[created["id"]]["phone"], "")
        self.assertEqual(by_id[created["id"]]["email"], "")

    def test_blank_contact_saved_as_empty_string(self):
        for index, (phone, email, description) in enumerate([
            ("", "", "空字符串"),
            ("   ", " \t ", "只有空白"),
        ]):
            with self.subTest(description=description):
                payload = make_payload(f"T31{index + 1}", phone=phone, email=email)
                status, created = request("POST", "/api/employees", payload)
                self.assertEqual(status, 201, f"联系方式{description}应允许建档: {created}")
                self.assertEqual(created["phone"], "")
                self.assertEqual(created["email"], "")

                by_id, _ = list_by_id()
                self.assertEqual(by_id[created["id"]], created)

    def test_contact_trimmed_without_format_validation(self):
        """有文字时只去两端空白，不增加号码/邮箱格式限制。"""
        payload = make_payload(
            "T313",
            phone="  分机 8008  ",
            email="  not-an-email  ",
        )
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"联系方式不做格式校验，应允许建档: {created}")
        self.assertEqual(created["phone"], "分机 8008")
        self.assertEqual(created["email"], "not-an-email")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]], created)

    def test_non_string_contact_rejected(self):
        """显式传入非字符串联系方式（含 null）返回 400，field 对应电话/邮箱。"""
        cases = [
            (None, "null"),
            (13800001234, "数字"),
            (True, "布尔值"),
            (["13800001234"], "数组"),
            ({"number": "138"}, "对象"),
        ]
        for field, label in (("phone", "电话"), ("email", "邮箱")):
            for index, (value, description) in enumerate(cases):
                with self.subTest(field=field, description=description):
                    employee_no = f"T32{0 if field == 'phone' else 1}{index}"
                    payload = make_payload(employee_no)
                    payload[field] = value
                    status, body = request("POST", "/api/employees", payload)
                    self.assertEqual(status, 400,
                                     f"{label}为{description}应返回 400: {body}")
                    self.assertEqual(body.get("field"), field,
                                     f"field 应指向{label}: {body}")
                    message = body.get("error", "")
                    self.assertIn(label, message)
                    self.assertIn("必须是字符串", message)

                    _, rows = list_by_id()
                    self.assertFalse(
                        any(row["employee_no"] == employee_no for row in rows),
                        f"{label}为{description}的失败提交不得新增档案",
                    )

    def test_null_contact_is_not_treated_as_omitted(self):
        """null 不等于未填写：省略可建档，显式 null 必须 400。"""
        omitted = make_payload("T330")
        del omitted["phone"]
        status, created = request("POST", "/api/employees", omitted)
        self.assertEqual(status, 201, f"省略电话应允许建档: {created}")
        self.assertEqual(created["phone"], "")

        explicit_null = make_payload("T331")
        explicit_null["phone"] = None
        status, body = request("POST", "/api/employees", explicit_null)
        self.assertEqual(status, 400, f"显式 null 电话应返回 400: {body}")
        self.assertEqual(body.get("field"), "phone")

        _, rows = list_by_id()
        self.assertFalse(
            any(row["employee_no"] == "T331" for row in rows),
            "显式 null 的失败提交不得新增档案",
        )


class FailedSubmissionPersistenceTest(unittest.TestCase):
    """校验失败的提交不新增残缺记录，已有档案内容与编号保持原样。"""

    def test_failed_submissions_leave_no_trace(self):
        baseline = make_payload("T400", name="基线员工·T400")
        status, created = request("POST", "/api/employees", baseline)
        self.assertEqual(status, 201, f"基线档案应建档成功: {created}")
        baseline_id = created["id"]

        snapshot_before, _ = list_by_id()
        self.assertIn(baseline_id, snapshot_before)

        # 覆盖各类校验失败：必填缺失/空白/非字符串、联系方式非字符串。
        failed_payloads = [
            make_payload("T401", name=""),
            make_payload("T402", department="   "),
            make_payload("T403", position=None),
            make_payload("T404", name=123),
            make_payload("T405", phone={"tel": "1"}),
            make_payload("T406", email=None),
        ]
        missing_name = make_payload("T407")
        del missing_name["name"]
        failed_payloads.append(missing_name)

        for payload in failed_payloads:
            status, body = request("POST", "/api/employees", payload)
            self.assertEqual(status, 400, f"非法提交应返回 400: {body}")
            self.assertIn("field", body)

        snapshot_after, rows_after = list_by_id()
        self.assertEqual(snapshot_after, snapshot_before,
                         "失败提交不得新增或改动任何档案")
        self.assertEqual(snapshot_after[baseline_id], created,
                         "已有档案的内容与编号必须保持原样")

        failed_numbers = {f"t40{i}" for i in range(1, 8)}
        present_numbers = {row["employee_no"].lower() for row in rows_after}
        self.assertTrue(
            failed_numbers.isdisjoint(present_numbers),
            f"失败工号不得出现在员工列表中: {sorted(failed_numbers & present_numbers)}",
        )
        self.assertFalse(
            any(not row.get("name") for row in rows_after),
            "失败提交不得留下姓名为空的残缺档案",
        )

        # 失败之后，合法提交仍可正常建档。
        status, created_ok = request("POST", "/api/employees",
                                     make_payload("T408", name="失败后新建·T408"))
        self.assertEqual(status, 201, f"失败提交后合法档案仍应建档成功: {created_ok}")
        final_by_id, _ = list_by_id()
        self.assertEqual(final_by_id[created_ok["id"]], created_ok)
        self.assertEqual(final_by_id[baseline_id], created, "基线档案仍保持原样")


if __name__ == "__main__":
    unittest.main(verbosity=2)
