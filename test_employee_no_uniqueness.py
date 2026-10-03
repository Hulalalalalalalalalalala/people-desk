#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""工号唯一性的端到端自动化回归测试。

通过真实 HTTP 进程验证公开行为（仅使用标准库）：
1. 工号去掉首尾空白后保存，全部员工中忽略英文字母大小写判重；
2. 首次录入去掉空白后的写法原样保留（不被统一改成小写）；
3. 重复工号返回 409（中文原因、field=employee_no），不覆盖、不新增；
4. 姓名相同但工号不同允许建档；工号只有空白返回 400；
5. 失败提交之后，新的合法工号仍可正常建档。
"""
import json
import os
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

# 首次建档内容与重复提交内容刻意使用完全不同的姓名/部门/联系方式，
# 以便一旦发生覆盖或意外新增，断言能直接暴露。
FIRST_EMPLOYEE = {
    "employee_no": " A007 ",
    "name": "首位建档·赵甲",
    "department": "研发部",
    "position": "后端工程师",
    "effective_date": "2026-09-01",
    "phone": "13800000001",
    "email": "first.a007@example.com",
}
FIRST_EMPLOYEE_NO = "A007"

DUP_EMPLOYEE = {
    "employee_no": "a007",
    "name": "重复提交·钱乙",
    "department": "市场部",
    "position": "实习生",
    "effective_date": "2026-10-02",
    "phone": "13900000002",
    "email": "duplicate.a007@example.com",
}

SAME_NAME_DIFFERENT_NO = {
    "employee_no": "A008",
    "name": FIRST_EMPLOYEE["name"],  # 与首条姓名相同，工号确实不同
    "department": "研发部",
    "position": "测试工程师",
    "effective_date": "2026-09-15",
    "phone": "13800000003",
    "email": "same.name.a008@example.com",
}

BLANK_NO_PAYLOAD = {
    "employee_no": "   ",
    "name": "空白工号·孙丙",
    "department": "行政部",
    "position": "助理",
    "effective_date": "2026-10-01",
    "phone": "13800000004",
    "email": "blank.no@example.com",
}

AFTER_FAILURE_PAYLOAD = {
    "employee_no": "A009",
    "name": "失败后新建·李丁",
    "department": "人事部",
    "position": "专员",
    "effective_date": "2026-10-03",
    "phone": "13800000005",
    "email": "after.failure.a009@example.com",
}

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


def assert_employee_no_conflict(testcase, status, body):
    testcase.assertEqual(status, 409, f"重复工号应返回 409: {body}")
    testcase.assertEqual(body.get("field"), "employee_no")
    message = body.get("error", "")
    testcase.assertIn("工号", message)
    testcase.assertIn("已存在", message)


class EmployeeNoUniquenessTest(unittest.TestCase):
    def test_employee_no_uniqueness_workflow(self):
        """建档成功 → 重复被拒 → 拒绝后记录状态 → 同名不同号 → 空白工号 → 失败后仍可建档。"""

        # 每个断言点都重新拉取列表，保证“保存的内容”和“查询出来的内容”一致。

        # 0. 全新数据目录，初始不应已有 A007。
        before, before_rows = list_by_id()
        self.assertEqual(before_rows, [], "测试数据目录应为空，确保可重复执行")

        # 1. 有效档案录入：工号“ A007 ”去掉首尾空白后以“A007”保存，保留首次写法。
        status, created = request("POST", "/api/employees", FIRST_EMPLOYEE)
        self.assertEqual(status, 201, f"首次建档应成功: {created}")
        self.assertIn("id", created)
        self.assertIsInstance(created["id"], int)
        first_id = created["id"]
        self.assertEqual(created["employee_no"], FIRST_EMPLOYEE_NO)
        self.assertEqual(created["name"], FIRST_EMPLOYEE["name"].strip())
        self.assertEqual(created["department"], FIRST_EMPLOYEE["department"].strip())
        self.assertEqual(created["position"], FIRST_EMPLOYEE["position"].strip())
        self.assertEqual(created["effective_date"], FIRST_EMPLOYEE["effective_date"])
        self.assertEqual(created["phone"], FIRST_EMPLOYEE["phone"])
        self.assertEqual(created["email"], FIRST_EMPLOYEE["email"])
        self.assertEqual(created["status"], "在职")

        # 查询出来的工号同样是“A007”，且该 id 的完整内容与创建响应逐字段一致。
        snapshot1, rows1 = list_by_id()
        self.assertEqual(sorted(snapshot1), [first_id])
        self.assertEqual(snapshot1[first_id], created)
        self.assertEqual(snapshot1[first_id]["employee_no"], FIRST_EMPLOYEE_NO)

        # 2. 小写工号“a007”重复录入：409，中文原因，字段 employee_no。
        status, body = request("POST", "/api/employees", DUP_EMPLOYEE)
        assert_employee_no_conflict(self, status, body)

        snapshot_after_dup1, _ = list_by_id()
        self.assertEqual(set(snapshot_after_dup1), {first_id}, "重复提交不得新增员工")
        self.assertEqual(
            snapshot_after_dup1, snapshot1, "重复提交不得覆盖首次保存的档案"
        )
        self.assertEqual(
            snapshot_after_dup1[first_id]["employee_no"],
            FIRST_EMPLOYEE_NO,
            "判重不得把首次保存的工号改成小写",
        )

        # 3. 首尾带空白的“ A007 ”重复录入（换另一姓名/部门/联系方式）：同样 409。
        padded = dict(DUP_EMPLOYEE)
        padded["employee_no"] = "  A007  "
        status, body = request("POST", "/api/employees", padded)
        assert_employee_no_conflict(self, status, body)

        snapshot_after_dup2, rows2 = list_by_id()
        self.assertEqual(set(snapshot_after_dup2), {first_id}, "重复提交不得新增员工")
        self.assertEqual(
            snapshot_after_dup2, snapshot1, "重复提交不得替换首次保存的档案内容"
        )
        self.assertEqual(snapshot_after_dup2[first_id]["name"], FIRST_EMPLOYEE["name"])
        self.assertEqual(
            snapshot_after_dup2[first_id]["department"],
            FIRST_EMPLOYEE["department"],
        )
        self.assertEqual(
            snapshot_after_dup2[first_id]["phone"], FIRST_EMPLOYEE["phone"]
        )
        self.assertEqual(
            snapshot_after_dup2[first_id]["email"], FIRST_EMPLOYEE["email"]
        )
        self.assertEqual([row["id"] for row in rows2], sorted(row["id"] for row in rows2))

        # 4. 姓名相同、工号确实不同（A008）：允许创建独立档案。
        status, created_a008 = request("POST", "/api/employees", SAME_NAME_DIFFERENT_NO)
        self.assertEqual(status, 201, f"工号不同时即使姓名相同也应建档成功: {created_a008}")
        a008_id = created_a008["id"]
        self.assertIsInstance(a008_id, int)
        self.assertNotEqual(a008_id, first_id)
        self.assertEqual(created_a008["employee_no"], "A008")
        self.assertEqual(created_a008["name"], FIRST_EMPLOYEE["name"])

        snapshot2, rows3 = list_by_id()
        self.assertEqual(set(snapshot2), {first_id, a008_id})
        self.assertEqual(snapshot2[first_id], created, "原 A007 档案必须保持原样")
        self.assertEqual(snapshot2[a008_id], created_a008)
        self.assertEqual(
            [row["id"] for row in rows3], sorted(row["id"] for row in rows3)
        )

        # 5. 工号只有空白：按现有校验返回 400，中文原因，字段 employee_no，不新增档案。
        status, body = request("POST", "/api/employees", BLANK_NO_PAYLOAD)
        self.assertEqual(status, 400, f"空白工号应返回 400: {body}")
        self.assertEqual(body.get("field"), "employee_no")
        self.assertIn("工号", body.get("error", ""))

        snapshot_after_blank, _ = list_by_id()
        self.assertEqual(
            snapshot_after_blank, snapshot2, "空白工号提交失败后不得留下新档案"
        )

        # 6. 此前的重复/无效提交不得影响后续合法建档：A009 仍应成功。
        status, created_a009 = request("POST", "/api/employees", AFTER_FAILURE_PAYLOAD)
        self.assertEqual(status, 201, f"失败提交后合法工号仍应建档成功: {created_a009}")
        a009_id = created_a009["id"]
        self.assertNotIn(a009_id, snapshot2)
        self.assertEqual(created_a009["employee_no"], "A009")

        final_by_id, final_rows = list_by_id()
        self.assertEqual(set(final_by_id), {first_id, a008_id, a009_id})
        self.assertEqual(final_by_id[first_id], created, "已有员工 A007 保持原样")
        self.assertEqual(final_by_id[a008_id], created_a008, "已有员工 A008 保持原样")
        self.assertEqual(final_by_id[a009_id], created_a009)
        self.assertFalse(
            any(row["name"] == DUP_EMPLOYEE["name"] for row in final_rows),
            "重复提交使用的另一姓名不得出现在员工列表中",
        )
        self.assertFalse(
            any(row["name"] == BLANK_NO_PAYLOAD["name"] for row in final_rows),
            "空白工号提交使用的姓名不得出现在员工列表中",
        )

    def test_lowercase_first_then_uppercase_is_conflict(self):
        """反向大小写：首次以小写 b010 建档应保留小写，再以 B010 提交应被拒绝。"""
        lowercase_payload = {
            "employee_no": " b010 ",
            "name": "小写首录·周戊",
            "department": "运维部",
            "position": "运维工程师",
            "effective_date": "2026-09-20",
            "phone": "13800000006",
            "email": "lower.b010@example.com",
        }
        status, created = request("POST", "/api/employees", lowercase_payload)
        self.assertEqual(status, 201, f"小写工号首次建档应成功: {created}")
        self.assertEqual(created["employee_no"], "b010")
        lower_id = created["id"]

        uppercase_payload = dict(lowercase_payload)
        uppercase_payload["employee_no"] = "B010"
        uppercase_payload["name"] = "大写重复·吴己"
        uppercase_payload["email"] = "upper.b010@example.com"
        status, body = request("POST", "/api/employees", uppercase_payload)
        assert_employee_no_conflict(self, status, body)

        by_id, rows = list_by_id()
        self.assertIn(lower_id, by_id)
        self.assertEqual(by_id[lower_id], created, "大写重复提交不得覆盖小写首录档案")
        self.assertEqual(
            [row for row in rows if row["employee_no"].lower() == "b010"],
            [created],
            "忽略大小写判重，b010/B010 只能有一条档案",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
