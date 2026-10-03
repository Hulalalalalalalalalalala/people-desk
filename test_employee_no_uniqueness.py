#!/usr/bin/env python3
"""员工档案工号唯一性的自动化回归测试。

以黑盒方式启动真实的 PeopleDesk 服务（python3 app.py serve），覆盖：
1. 正常建档：工号去首尾空白后保存、保留首次录入的大小写写法、列表可查且内容一致；
2. 重复拒绝：忽略英文字母大小写与首尾空白，返回 409 及中文原因/employee_no 字段，
   且不覆盖首次档案、不留下额外员工；
3. 同名不同工号允许建档，两条记录各自有独立 id；
4. 工号只有空白返回 400 及中文原因/employee_no 字段，不产生无工号档案；
5. 重复或无效提交后，新的合法工号仍可正常建档。

运行方式：
    python3 -m unittest test_employee_no_uniqueness -v
或：
    python3 test_employee_no_uniqueness.py
"""
import http.client
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

APP_PATH = Path(__file__).resolve().parent / "app.py"


def valid_payload(**overrides):
    """一份完整合法的建档请求，测试数据可明确区分首次保存与重复提交。"""
    payload = {
        "employee_no": " A007 ",
        "name": "赵一鸣",
        "department": "研发部",
        "position": "后端工程师",
        "effective_date": "2026-09-01",
        "phone": "138-0000-0001",
        "email": "zhao.yiming@example.com",
    }
    payload.update(overrides)
    return payload


# 重复提交时使用的另一套姓名/部门/联系方式，便于发现误覆盖或意外新增。
DUPLICATE_OVERRIDES = {
    "name": "钱尔茂",
    "department": "市场部",
    "position": "市场专员",
    "phone": "139-0000-0002",
    "email": "qian.ermao@example.com",
}


class EmployeeNoUniquenessTests(unittest.TestCase):
    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory(prefix="peopledesk-test-")
        self.addCleanup(self._tmpdir.cleanup)
        self.proc = subprocess.Popen(
            [
                sys.executable,
                str(APP_PATH),
                "serve",
                "--host", "127.0.0.1",
                "--port", "0",  # 让系统分配可用端口，避免端口冲突
                "--data-dir", self._tmpdir.name,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self._stop_server)
        line = self.proc.stdout.readline()
        match = re.search(r"http://127\.0\.0\.1:(\d+)", line)
        if match is None:
            self.fail(f"服务未能启动，输出为：{line!r}，错误：{self.proc.stderr.read()}")
        self.port = int(match.group(1))

    def _stop_server(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self.proc.stdout.close()
        self.proc.stderr.close()

    def request(self, method, path, body=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {}
        data = None
        if body is not None:
            data = json.dumps(body, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json; charset=utf-8"
        conn.request(method, path, body=data, headers=headers)
        resp = conn.getresponse()
        raw = resp.read().decode("utf-8")
        conn.close()
        parsed = json.loads(raw) if raw else None
        return resp.status, parsed

    def get_employees(self):
        status, body = self.request("GET", "/api/employees")
        self.assertEqual(status, 200)
        self.assertIsInstance(body, dict)
        self.assertIn("employees", body)
        return body["employees"]

    def create(self, **overrides):
        return self.request("POST", "/api/employees", valid_payload(**overrides))

    def assert_employee_no_error(self, body, expected_reason):
        self.assertIsInstance(body, dict)
        self.assertEqual(body.get("field"), "employee_no")
        self.assertIn("工号", body.get("error", ""))
        self.assertIn(expected_reason, body.get("error", ""))

    def test_valid_employee_is_trimmed_preserves_case_and_appears_in_list(self):
        """工号“ A007 ”应创建成功，保存为“A007”，列表内容与创建结果完全一致。"""
        status, created = self.create()
        self.assertEqual(status, 201, created)
        self.assertIsInstance(created, dict)
        self.assertIsInstance(created.get("id"), int)
        # 去掉首尾空白，但保留首次录入的大小写写法，不能统一成小写。
        self.assertEqual(created["employee_no"], "A007")
        self.assertEqual(created["name"], "赵一鸣")
        self.assertEqual(created["department"], "研发部")
        self.assertEqual(created["position"], "后端工程师")
        self.assertEqual(created["effective_date"], "2026-09-01")
        self.assertEqual(created["phone"], "138-0000-0001")
        self.assertEqual(created["email"], "zhao.yiming@example.com")
        self.assertEqual(created["status"], "在职")

        employees = self.get_employees()
        self.assertEqual(len(employees), 1)
        # 列表中通过服务生成的 id 找到该档案，内容须与创建响应逐条一致。
        matching = [e for e in employees if e["id"] == created["id"]]
        self.assertEqual(len(matching), 1)
        self.assertEqual(matching[0], created)
        self.assertEqual(matching[0]["employee_no"], "A007")

    def test_duplicate_employee_no_rejected_without_mutating_first_record(self):
        """小写“a007”与带空白的“ A007 ”再次录入均返回 409，首次档案原样保留。"""
        status, created = self.create()
        self.assertEqual(status, 201, created)
        original_id = created["id"]

        for duplicate_no in ("a007", " A007 ", "  a007  "):
            with self.subTest(employee_no=duplicate_no):
                status, body = self.create(employee_no=duplicate_no, **DUPLICATE_OVERRIDES)
                self.assertEqual(status, 409, body)
                self.assert_employee_no_error(body, "已存在")

        employees = self.get_employees()
        # 重复提交不能新增员工，也不能用另一套姓名/部门/联系方式覆盖首次档案。
        self.assertEqual(len(employees), 1)
        self.assertEqual(employees[0], created)
        self.assertEqual(employees[0]["id"], original_id)
        self.assertEqual(employees[0]["employee_no"], "A007")
        self.assertEqual(employees[0]["name"], "赵一鸣")
        self.assertEqual(employees[0]["department"], "研发部")
        self.assertEqual(employees[0]["phone"], "138-0000-0001")
        self.assertEqual(employees[0]["email"], "zhao.yiming@example.com")
        self.assertFalse(
            any(e["name"] == DUPLICATE_OVERRIDES["name"] for e in employees),
            "重复提交的姓名不应出现在任何档案中",
        )

    def test_same_name_with_different_employee_no_is_allowed(self):
        """姓名相同但工号不同（A008）应允许建档，两条记录各有独立 id。"""
        status, first = self.create(name="孙三多")
        self.assertEqual(status, 201, first)
        status, second = self.create(employee_no="A008", name="孙三多")
        self.assertEqual(status, 201, second)

        self.assertNotEqual(first["id"], second["id"])
        self.assertEqual(first["employee_no"], "A007")
        self.assertEqual(second["employee_no"], "A008")

        employees = self.get_employees()
        self.assertEqual(len(employees), 2)
        self.assertEqual({e["id"] for e in employees}, {first["id"], second["id"]})
        self.assertEqual({e["employee_no"] for e in employees}, {"A007", "A008"})
        self.assertTrue(all(e["name"] == "孙三多" for e in employees))

    def test_blank_employee_no_rejected_and_no_record_created(self):
        """工号只有空白应返回 400 与 employee_no 字段，列表中不得出现无有效工号的档案。"""
        status, body = self.create(employee_no="   ")
        self.assertEqual(status, 400, body)
        self.assert_employee_no_error(body, "不能为空")
        self.assertEqual(self.get_employees(), [])

        # 无效提交之后，新的合法工号仍应能正常建档。
        status, created = self.create(employee_no="A009")
        self.assertEqual(status, 201, created)
        self.assertEqual(created["employee_no"], "A009")
        employees = self.get_employees()
        self.assertEqual(len(employees), 1)
        self.assertEqual(employees[0], created)

    def test_valid_employee_no_still_works_after_duplicate_and_invalid_submissions(self):
        """重复与空白提交之后，新的合法工号 A008 应正常创建，已有档案保持原样。"""
        status, first = self.create()
        self.assertEqual(status, 201, first)

        status, body = self.create(employee_no="a007", **DUPLICATE_OVERRIDES)
        self.assertEqual(status, 409, body)
        self.assert_employee_no_error(body, "已存在")

        status, body = self.create(employee_no="   ")
        self.assertEqual(status, 400, body)
        self.assert_employee_no_error(body, "不能为空")

        status, second = self.create(employee_no="A008", name="李四方", department="产品部")
        self.assertEqual(status, 201, second)
        self.assertEqual(second["employee_no"], "A008")
        self.assertNotEqual(first["id"], second["id"])

        employees = self.get_employees()
        self.assertEqual(len(employees), 2)
        by_id = {e["id"]: e for e in employees}
        self.assertEqual(set(by_id), {first["id"], second["id"]})
        # 首次档案仍是首次保存的 id 与内容，未被重复提交替换。
        self.assertEqual(by_id[first["id"]], first)
        self.assertEqual(by_id[second["id"]], second)
        self.assertFalse(
            any(e["name"] == DUPLICATE_OVERRIDES["name"] for e in employees),
            "重复提交的姓名不应出现在任何档案中",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
