#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""新建员工档案文字字段校验的端到端自动化回归测试。

首页表单虽然会提示必填，但直接调用 POST /api/employees 建档时，服务端
同样必须执行这些业务限制。本模块通过真实 HTTP 进程验证公开行为（仅使用
标准库），沿用建档与员工列表查询接口，重点保护必填文字、选填联系方式和
失败提交对档案的影响：

1. 工号、姓名、部门、岗位必须是字符串，去掉首尾空白后仍有内容才建档；
   资料合法时返回 201，创建响应与随后员工列表查询中保存的都是“去掉首尾
   空白后”的文字。文字内部的空格、英文字母大小写和中文原文不得被顺便
   改写：姓名“  陈  明  ”保存为“陈  明”，岗位内部空格保留；全角空格
   也属于首尾空白。任职状态固定“在职”，员工编号由服务生成。
2. 必填文字字段未提供、为空字符串或只有空白（含制表符/换行/全角空格）
   时返回 400，用中文说明对应资料不能为空，field 指向实际出错的字段；
   姓名、部门、岗位逐一保障，不能只依赖已有的空白工号案例。
3. 必填文字字段显式传入 null、数字、布尔值、数组或对象时返回 400，
   说明该字段必须是字符串，不能把这些值转换成文字后建档，field 同样
   指向实际出错的字段。
4. 电话和邮箱继续选填：省略、空字符串或只有空白时允许建档，并以空字符串
   保存；有文字时只去掉两端空白、保留内部空格与原文大小写，不增加号码
   或邮箱格式限制。显式传入非字符串（含 null）仍返回 400，中文原因与
   field 对应电话或邮箱，null 不能被当作“未填写”处理。
5. 任何上述校验失败的提交都不得新增残缺记录：员工列表不新增、已有档案
   （内容与编号）保持原样；失败之后用合法资料仍可正常建档。
6. 工号唯一性（409）与生效日期（400）等既有规则继续成立；首页页面提交
   行为仍由 test_homepage_employee_form.py 与 test_special_text_display.py
   覆盖，本模块不改变任何建档规则。
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

REQUIRED_TEXT_FIELDS = ("employee_no", "name", "department", "position")
FIELD_LABELS = {
    "employee_no": "工号",
    "name": "姓名",
    "department": "部门",
    "position": "岗位",
    "effective_date": "生效日期",
    "phone": "电话",
    "email": "邮箱",
}

# 显式非字符串取值：任何一个都不允许被转换成文字后建档。
NON_STRING_VALUES = (
    (None, "null"),
    (0, "数字零"),
    (123, "非零数字"),
    (True, "布尔真"),
    (False, "布尔假"),
    (["工程师"], "数组"),
    ({"role": "工程师"}, "对象"),
)


def make_payload(employee_no, **overrides):
    """构造一份除指定字段外均符合现有录入规则的建档请求。"""
    payload = {
        "employee_no": employee_no,
        "name": f"文字测试·{employee_no}",
        "department": " 研发部 ",
        "position": " 后端工程师 ",
        "effective_date": "2026-10-01",
        "phone": "13800000001",
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


def assert_blank_error(testcase, field, status, body):
    """缺失/空串/纯空白：400 + 中文“不能为空”原因 + field 指向实际字段。"""
    label = FIELD_LABELS[field]
    testcase.assertEqual(status, 400, f"{label}为空时应返回 400: {body}")
    testcase.assertEqual(body.get("field"), field, f"field 必须指向实际出错的 {field}")
    message = body.get("error", "")
    testcase.assertIn(label, message, "错误原因必须用中文点明对应资料")
    testcase.assertIn("不能为空", message, f"{label}为空应说明不能为空: {message!r}")


def assert_type_error(testcase, field, status, body):
    """非字符串（含 null）：400 + 中文“必须是字符串”原因 + field 指向实际字段。"""
    label = FIELD_LABELS[field]
    testcase.assertEqual(status, 400, f"{label}为非字符串时应返回 400: {body}")
    testcase.assertEqual(body.get("field"), field, f"field 必须指向实际出错的 {field}")
    message = body.get("error", "")
    testcase.assertIn(label, message, "错误原因必须用中文点明对应资料")
    testcase.assertIn("必须是字符串", message, f"{label}类型错误应说明必须是字符串: {message!r}")
    # 提供了（错误类型的）值与“未填写”是两类问题，错误原因不能混为一谈。
    testcase.assertNotIn("不能为空", message,
                         f"{label}是非字符串时不应报“为空”，而应报类型错误: {message!r}")


class ValidTrimmedTextTest(unittest.TestCase):
    """合法文字：去掉首尾空白后保存，内部空格/大小写/中文原文不被改写。"""

    def test_trimmed_text_saved_in_response_and_list(self):
        """姓名“  陈  明  ”保存为“陈  明”，岗位等字段的内部空格保留。"""
        payload = {
            "employee_no": " T101 ",
            "name": "  陈  明  ",
            "department": " 研发 平台部 ",
            "position": " 高级 软件工程师 ",
            "effective_date": "2026-10-01",
            "phone": " 138 0000 0001 ",
            "email": " Chen.Ming@Example.com ",
        }
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"合法档案应建档成功: {created}")

        # 员工编号由服务生成，任职状态固定在职。
        self.assertIsInstance(created.get("id"), int)
        self.assertEqual(created["status"], "在职")

        # 创建响应中保存的是去掉首尾空白后的文字，内部内容原样保留。
        self.assertEqual(created["employee_no"], "T101")
        self.assertEqual(created["name"], "陈  明", "首尾空白去掉，内部两个空格必须保留")
        self.assertEqual(created["department"], "研发 平台部", "部门内部空格必须保留")
        self.assertEqual(created["position"], "高级 软件工程师", "岗位内部空格必须保留")
        self.assertEqual(created["effective_date"], "2026-10-01")
        self.assertEqual(created["phone"], "138 0000 0001", "联系方式同样只去首尾空白")
        self.assertEqual(
            created["email"], "Chen.Ming@Example.com",
            "邮箱不得新增大小写或格式改写",
        )

        # 随后查询到的档案与创建响应逐字段一致，文字处理结果已落库。
        by_id, rows = list_by_id()
        self.assertIn(created["id"], by_id)
        self.assertEqual(by_id[created["id"]], created)
        stored = by_id[created["id"]]
        self.assertEqual(stored["name"], "陈  明")
        self.assertEqual(stored["department"], "研发 平台部")
        self.assertEqual(stored["position"], "高级 软件工程师")
        self.assertEqual([row["id"] for row in rows], sorted(row["id"] for row in rows))

    def test_internal_spaces_case_and_chinese_not_rewritten(self):
        """全角首尾空白被去掉，内部空格、英文大小写和繁体中文原文都不得被改写。"""
        payload = {
            "employee_no": " tX-102 ",
            "name": "　李 雷　",                # 全角空格在首尾，内部半角空格保留
            "department": " 數據（繁體）平臺 ",   # 繁体中文不得被简化或替换
            "position": " SRE Lead 值班 ",       # 英文大小写与内部空格保留
            "effective_date": "2026-09-20",
            "phone": "",
            "email": "",
        }
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"合法档案应建档成功: {created}")
        self.assertEqual(created["employee_no"], "tX-102", "工号英文大小写必须保留首次写法")
        self.assertEqual(created["name"], "李 雷")
        self.assertEqual(created["department"], "數據（繁體）平臺")
        self.assertEqual(created["position"], "SRE Lead 值班")
        self.assertEqual(created["status"], "在职")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]], created, "列表查询应保存同样的处理后文字")


class BlankRequiredTextTest(unittest.TestCase):
    """必填文字未提供、空字符串或只有空白：400 + 中文不能为空 + 实际 field。"""

    def test_blank_required_fields_rejected(self):
        # 工号、姓名、部门、岗位逐一保障，不只依赖已有的空白工号案例。
        case_no = 0
        for field in REQUIRED_TEXT_FIELDS:
            variants = [
                ("missing", "未提供该字段"),
                ("", "空字符串"),
                ("   ", "只有半角空格"),
                ("\t\n ", "只有制表符/换行/空格"),
                ("　　", "只有全角空格"),
            ]
            for value, description in variants:
                case_no += 1
                with self.subTest(field=field, description=description):
                    # 每次使用全新工号，保证被校验的字段是提交中唯一的问题。
                    payload = make_payload(f"B{case_no:03d}")
                    if value == "missing":
                        del payload[field]
                    else:
                        payload[field] = value
                    status, body = request("POST", "/api/employees", payload)
                    assert_blank_error(self, field, status, body)


class NonStringRequiredTextTest(unittest.TestCase):
    """必填文字为 null/数字/布尔/数组/对象：400 + 必须是字符串 + 实际 field。"""

    def test_non_string_required_fields_rejected(self):
        case_no = 0
        for field in REQUIRED_TEXT_FIELDS:
            for value, description in NON_STRING_VALUES:
                case_no += 1
                with self.subTest(field=field, value=description):
                    # 全新工号 + 其余字段合法：被改写类型的字段是唯一问题，
                    # 例如 name=123 时 field 必须是 name，而不是 employee_no。
                    payload = make_payload(f"C{case_no:03d}")
                    payload[field] = value
                    status, body = request("POST", "/api/employees", payload)
                    assert_type_error(self, field, status, body)


class OptionalContactTest(unittest.TestCase):
    """电话、邮箱选填：省略/空白存空串，有值只 trim，显式非字符串（含 null）拒绝。"""

    def test_omitted_contacts_saved_as_empty_string(self):
        """完全省略电话和邮箱：允许建档，响应与列表中均保存为空字符串。"""
        payload = make_payload("O201")
        del payload["phone"]
        del payload["email"]
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"省略联系方式应允许建档: {created}")
        self.assertEqual(created["phone"], "")
        self.assertEqual(created["email"], "")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]]["phone"], "")
        self.assertEqual(by_id[created["id"]]["email"], "")

    def test_blank_contacts_saved_as_empty_string(self):
        """空字符串或只有空白的联系方式允许建档，并作为空字符串保存。"""
        cases = [
            ("O202", "", "  "),
            ("O203", "\t　", ""),
            ("O204", "　", "\n\t "),
        ]
        for employee_no, phone, email in cases:
            with self.subTest(employee_no=employee_no):
                payload = make_payload(employee_no, phone=phone, email=email)
                status, created = request("POST", "/api/employees", payload)
                self.assertEqual(status, 201, f"空白联系方式应允许建档: {created}")
                self.assertEqual(created["phone"], "", f"电话 {phone!r} 应保存为空字符串")
                self.assertEqual(created["email"], "", f"邮箱 {email!r} 应保存为空字符串")

                by_id, _ = list_by_id()
                stored = by_id[created["id"]]
                self.assertEqual(stored["phone"], "")
                self.assertEqual(stored["email"], "")
                self.assertEqual(stored, created)

    def test_contact_text_only_trimmed_without_format_restriction(self):
        """有文字时只去掉两端空白；不增加号码或邮箱格式限制，内部空格与大小写保留。"""
        payload = {
            "employee_no": "O205",
            "name": "联系方式测试·O205",
            "department": "行政部",
            "position": "前台",
            "effective_date": "2026-10-01",
            "phone": " 分机 001（前台） ",      # 非典型号码格式，内部有空格
            "email": " 不是邮箱·地址 ",          # 根本不是邮箱格式，也不得拒绝
        }
        status, created = request("POST", "/api/employees", payload)
        self.assertEqual(status, 201, f"联系方式不做格式校验，应建档成功: {created}")
        self.assertEqual(created["phone"], "分机 001（前台）")
        self.assertEqual(created["email"], "不是邮箱·地址")

        payload2 = make_payload(
            "O206",
            name="联系方式测试·O206",
            phone="138-0000-0000 转 9",
            email="Foo.Bar@EXAMPLE.COM",
        )
        status, created2 = request("POST", "/api/employees", payload2)
        self.assertEqual(status, 201, f"含分隔符/大小写的联系方式应原样保存: {created2}")
        self.assertEqual(created2["phone"], "138-0000-0000 转 9", "内部空格必须保留")
        self.assertEqual(created2["email"], "Foo.Bar@EXAMPLE.COM", "大小写不得被改写")

        by_id, _ = list_by_id()
        self.assertEqual(by_id[created["id"]], created)
        self.assertEqual(by_id[created2["id"]], created2)

    def test_non_string_phone_rejected(self):
        """电话显式传入非字符串（含 null）：400 + field=phone，null 不能当未填写。"""
        for value, description in NON_STRING_VALUES:
            with self.subTest(value=description):
                payload = make_payload("O210", phone=value, email="valid@example.com")
                status, body = request("POST", "/api/employees", payload)
                assert_type_error(self, "phone", status, body)

    def test_non_string_email_rejected(self):
        """邮箱显式传入非字符串（含 null）：400 + field=email，null 不能当未填写。"""
        for index, (value, description) in enumerate(NON_STRING_VALUES):
            with self.subTest(value=description):
                # 工号互不相同仅为便于定位；这些提交本就不应落库。
                payload = make_payload(f"O22{index}", phone="13800000099", email=value)
                status, body = request("POST", "/api/employees", payload)
                assert_type_error(self, "email", status, body)


class FailedSubmissionPersistenceTest(unittest.TestCase):
    """校验失败的提交：不新增残缺记录、不覆盖已有档案，之后合法建档仍成功。"""

    def test_failed_submissions_leave_no_trace(self):
        baseline = make_payload(
            "P301",
            name="基线档案·P301",
            department="人事部",
            position="HRBP",
            phone="13600000001",
            email="p301@example.com",
        )
        status, baseline_created = request("POST", "/api/employees", baseline)
        self.assertEqual(status, 201, f"基线档案应建档成功: {baseline_created}")
        baseline_id = baseline_created["id"]

        snapshot_before, rows_before = list_by_id()
        self.assertIn(baseline_id, snapshot_before)

        # 覆盖各类文字校验失败：必填为空、必填非字符串、联系方式非字符串。
        failures = [
            ("P310", {"name": ""}, "blank"),
            ("P311", {"name": "   "}, "blank"),
            ("P312", {"name": None}, "type"),
            ("P313", {"department": ["研发部"]}, "type"),
            ("P314", {"position": 42}, "type"),
            ("P315", {"employee_no": "\t"}, "blank"),
            ("P316", {"employee_no": False}, "type"),
            ("P317", {"phone": None}, "type"),
            ("P318", {"phone": 13800000000}, "type"),
            ("P319", {"email": {"addr": "x"}}, "type"),
            ("P320", {"email": None}, "type"),
        ]
        for employee_no, changes, kind in failures:
            field = next(iter(changes))
            payload = make_payload(employee_no, name=f"失败提交·{employee_no}")
            payload.update(changes)
            status, body = request("POST", "/api/employees", payload)
            if kind == "blank":
                assert_blank_error(self, field, status, body)
            else:
                assert_type_error(self, field, status, body)

            # 每次失败后立即核对：列表与失败前完全一致，无新增也无覆盖。
            snapshot_now, _ = list_by_id()
            self.assertEqual(
                snapshot_now, snapshot_before,
                f"{employee_no} 的失败提交不得改变员工列表（字段 {field}）",
            )

        snapshot_after, rows_after = list_by_id()

        # 全部失败提交后列表与失败前完全一致：已有档案的编号与全部内容保持原样。
        self.assertEqual(snapshot_after, snapshot_before)
        self.assertEqual(snapshot_after[baseline_id], baseline_created)
        self.assertEqual(snapshot_after[baseline_id]["name"], "基线档案·P301")
        self.assertEqual(snapshot_after[baseline_id]["phone"], "13600000001")
        self.assertEqual(snapshot_after[baseline_id]["email"], "p301@example.com")

        # 失败提交使用的姓名/工号一律不得残留，列表中不允许残缺记录。
        self.assertFalse(
            any(row["name"].startswith("失败提交·") for row in rows_after),
            "失败提交使用的姓名不得出现在员工列表中",
        )
        failed_numbers = {f"p31{i}" for i in range(0, 10)} | {"p320"}
        present_numbers = {row["employee_no"].strip().lower() for row in rows_after}
        self.assertTrue(
            failed_numbers.isdisjoint(present_numbers),
            f"失败工号不得出现在员工列表中: {sorted(failed_numbers & present_numbers)}",
        )
        for row in rows_after:
            for field in REQUIRED_TEXT_FIELDS:
                self.assertTrue(
                    isinstance(row[field], str) and row[field].strip(),
                    f"列表中不得存在 {field} 为空的残缺档案: {row}",
                )

        # 此前的失败提交不得影响后续合法建档。
        good = make_payload(
            "P321",
            name="失败后新建·P321",
            department="人事部",
            position="招聘专员",
            phone="",
            email="",
        )
        status, good_created = request("POST", "/api/employees", good)
        self.assertEqual(status, 201, f"失败提交后合法资料仍应建档成功: {good_created}")
        self.assertNotEqual(good_created["id"], baseline_id)
        self.assertEqual(good_created["status"], "在职")

        final_by_id, final_rows = list_by_id()
        self.assertEqual(set(final_by_id), set(snapshot_before) | {good_created["id"]})
        self.assertEqual(final_by_id[baseline_id], baseline_created, "已有档案必须保持原样")
        self.assertEqual(final_by_id[good_created["id"]], good_created)
        self.assertEqual([row["id"] for row in final_rows], sorted(row["id"] for row in final_rows))


class ExistingRulesRegressionTest(unittest.TestCase):
    """文字校验保障不得削弱工号唯一性与生效日期等既有规则。"""

    def test_duplicate_employee_no_still_conflicts(self):
        """同工号（忽略大小写、去首尾空白）重复建档仍返回 409，且不新增不覆盖。"""
        first = make_payload(
            "R401", name="唯一性基线·R401", department="研发部", position="架构师"
        )
        status, created = request("POST", "/api/employees", first)
        self.assertEqual(status, 201, f"首次建档应成功: {created}")

        snapshot_before, _ = list_by_id()

        duplicate = make_payload(
            " r401 ", name="重复工号·不应保存", department="市场部", position="实习生"
        )
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

    def test_invalid_effective_date_still_rejected(self):
        """所有文字字段合法但日期无效时，仍返回 400 且 field=effective_date。"""
        payload = make_payload("R402", effective_date="2026-02-29")
        status, body = request("POST", "/api/employees", payload)
        self.assertEqual(status, 400, f"非法日期应返回 400: {body}")
        self.assertEqual(body.get("field"), "effective_date")
        self.assertIn("日期", body.get("error", ""))

        by_id, _ = list_by_id()
        self.assertFalse(
            any(row["employee_no"] == "R402" for row in by_id.values()),
            "日期校验失败的提交不得留下档案",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
