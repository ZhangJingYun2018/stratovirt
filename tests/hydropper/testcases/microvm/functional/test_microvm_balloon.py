# Copyright (c) 2021 Huawei Technologies Co.,Ltd. All rights reserved.
#
# StratoVirt is licensed under Mulan PSL v2.
# You can use this software according to the terms and conditions of the Mulan
# PSL v2.
# You may obtain a copy of Mulan PSL v2 at:
#         http:#license.coscl.org.cn/MulanPSL2
# THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
# KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
# NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
# See the Mulan PSL v2 for more details.

"""Test microvm balloon"""
import time
import logging
import pytest

LOG_FORMAT = "%(asctime)s - %(levelname)s - %(message)s"
logging.basicConfig(filename='/var/log/pytest.log', level=logging.DEBUG, format=LOG_FORMAT)

@pytest.mark.acceptance
def test_microvm_balloon_query(microvm):
    """
    Test qmp command of querying balloon

    steps:
    1) launch microvm with argument: "-balloon deflate-on-oom=true".
    2) query the memory size, and check if it is 2524971008 which is the default memory size.
    """
    test_vm = microvm
    test_vm.basic_config(balloon=True, deflate_on_oom=True)
    test_vm.launch()
    resp = test_vm.query_balloon()
    assert int(resp["return"]["actual"]) == int(microvm.memsize) * 1024 * 1024

@pytest.mark.acceptance
def test_microvm_balloon(microvm):
    """
    Test qmp command of setting balloon

    steps:
    1) launch microvm with argument: "-balloon deflate-on-oom=true".
    2) query memory size, and save.
    3) set memory size through balloon device to 814748368.
    4) wait 5 seconds for ballooning.
    5) check if the memory size is less than 2524971008.
    6) set memory size through balloon device to 2524971008, and wait.
    7) check if the memory size is 2524971008.
    Note that balloon device may not inflate as many as the given argument, but it can deflate until
    no page left in balloon device. Therefore, memory in step 5 is less than 2524971008,
    while that in step 7 equals 2524971008.

    """
    test_vm = microvm
    test_vm.basic_config(balloon=True, deflate_on_oom=True)
    test_vm.launch()
    resp = test_vm.query_balloon()
    ori = int(resp["return"]["actual"])

    resp = test_vm.balloon_set(value=814748368)
    time.sleep(5)
    test_vm.event_wait(name='BALLOON_CHANGED', timeout=2.0)
    resp = test_vm.query_balloon()
    set1 = int(resp["return"]["actual"])
    assert set1 < 2524971008

    resp = test_vm.balloon_set(value=2524971008)
    time.sleep(5)
    resp = test_vm.query_balloon()
    logging.debug(resp)
    set2 = int(resp["return"]["actual"])
    assert ori == set2

@pytest.mark.acceptance
def test_microvm_balloon_active(microvm):
    """
    Test qmp command of setting balloon

    steps:
    1) launch microvm without active balloon device.
    2) check if balloon device is activated.
    """
    test_vm = microvm
    test_vm.basic_config()
    test_vm.launch()
    resp = test_vm.query_balloon()
    assert resp["error"]["desc"] == "No balloon device has been activated"
    resp = test_vm.balloon_set(value=2524971008)
    assert resp["error"]["desc"] == "No balloon device has been activated"
