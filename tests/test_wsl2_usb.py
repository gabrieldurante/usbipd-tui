import json
import subprocess
from types import SimpleNamespace

import pytest

import wsl2_usb
from wsl2_usb import Device


def make_device(**overrides) -> Device:
    values = {
        "bus_id": "1-2",
        "client_ip": None,
        "description": "Test USB device",
        "instance_id": r"USB\VID_1234&PID_ABCD\SERIAL",
        "is_forced": False,
        "persisted_guid": None,
        "stub_instance_id": None,
    }
    values.update(overrides)
    return Device(**values)


@pytest.mark.parametrize(
    ("overrides", "expected"),
    [
        ({}, "Not Shared"),
        ({"persisted_guid": "guid"}, "Bound"),
        ({"bus_id": None, "persisted_guid": "guid"}, "Persisted"),
        ({"client_ip": "172.20.0.2", "persisted_guid": "guid"}, "Attached"),
    ],
)
def test_device_state(overrides, expected):
    assert make_device(**overrides).state == expected


@pytest.mark.parametrize(
    ("state", "expected_actions"),
    [
        ("Not Shared", {"can_bind"}),
        ("Bound", {"can_attach", "can_unbind"}),
        ("Attached", {"can_detach", "can_unbind"}),
        ("Persisted", {"can_unbind"}),
    ],
)
def test_device_actions_match_state(state, expected_actions):
    overrides = {
        "Not Shared": {},
        "Bound": {"persisted_guid": "guid"},
        "Attached": {"persisted_guid": "guid", "client_ip": "172.20.0.2"},
        "Persisted": {"bus_id": None, "persisted_guid": "guid"},
    }[state]
    device = make_device(**overrides)

    actions = {
        "can_bind": device.can_bind(),
        "can_attach": device.can_attach(),
        "can_detach": device.can_detach(),
        "can_unbind": device.can_unbind(),
    }

    assert {action for action, allowed in actions.items() if allowed} == expected_actions


def test_device_vid_pid_is_normalized_to_uppercase():
    device = make_device(instance_id=r"USB\VID_1a2b&PID_3c4d\SERIAL")

    assert device.vid_pid == "1A2B:3C4D"


def test_device_vid_pid_is_missing_when_instance_id_has_no_hardware_id():
    assert make_device(instance_id="unrecognized").vid_pid == "—"


def test_device_row_key_uses_first_available_identifier():
    assert make_device().row_key == "1-2"
    assert make_device(bus_id=None, persisted_guid="guid").row_key == "guid"
    assert (
        make_device(bus_id=None, persisted_guid=None, instance_id="instance").row_key
        == "instance"
    )


def test_fetch_devices_parses_usbipd_state(monkeypatch):
    state = {
        "Devices": [
            {
                "BusId": "2-3",
                "ClientIPAddress": "172.20.0.2",
                "Description": "USB serial adapter",
                "InstanceId": r"USB\VID_0403&PID_6001\A",
                "IsForced": True,
                "PersistedGuid": "device-guid",
                "StubInstanceId": "stub-id",
            }
        ]
    }

    def fake_run(command, **kwargs):
        assert command == ["usbipd.exe", "state"]
        assert kwargs == {"capture_output": True, "text": True, "timeout": 10}
        return SimpleNamespace(stdout=json.dumps(state))

    monkeypatch.setattr(wsl2_usb.subprocess, "run", fake_run)

    assert wsl2_usb._fetch_devices() == [
        Device(
            bus_id="2-3",
            client_ip="172.20.0.2",
            description="USB serial adapter",
            instance_id=r"USB\VID_0403&PID_6001\A",
            is_forced=True,
            persisted_guid="device-guid",
            stub_instance_id="stub-id",
        )
    ]


@pytest.mark.parametrize("stdout", ["not json", "[]"])
def test_fetch_devices_returns_empty_list_for_invalid_state(monkeypatch, stdout):
    monkeypatch.setattr(
        wsl2_usb.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(stdout=stdout),
    )

    assert wsl2_usb._fetch_devices() == []


def test_run_usbipd_returns_command_result(monkeypatch):
    def fake_run(command, **kwargs):
        assert command == ["usbipd.exe", "attach", "--busid", "1-2", "--wsl"]
        assert kwargs == {"capture_output": True, "text": True, "timeout": 30}
        return SimpleNamespace(returncode=0, stdout="attached", stderr="")

    monkeypatch.setattr(wsl2_usb.subprocess, "run", fake_run)

    assert wsl2_usb._run_usbipd(
        "attach", "--busid", "1-2", "--wsl"
    ) == (0, "attached", "")


def test_run_usbipd_reports_timeout(monkeypatch):
    def time_out(*args, **kwargs):
        raise subprocess.TimeoutExpired("usbipd.exe", 30)

    monkeypatch.setattr(wsl2_usb.subprocess, "run", time_out)

    assert wsl2_usb._run_usbipd("list") == (1, "", "Command timed out")


def test_admin_commands_are_delegated_to_elevated_runner(monkeypatch):
    calls = []

    def fake_elevated(*args):
        calls.append(args)
        return 0, "bound", ""

    monkeypatch.setattr(wsl2_usb, "_run_elevated", fake_elevated)

    assert wsl2_usb._run_usbipd("bind", "--busid", "1-2") == (0, "bound", "")
    assert calls == [("bind", "--busid", "1-2")]


def test_auto_attach_bus_ids_round_trip(monkeypatch, tmp_path):
    config_dir = tmp_path / "wsl2_usb"
    config_file = config_dir / "auto_attach_busids.json"
    monkeypatch.setattr(wsl2_usb, "CONFIG_DIR", config_dir)
    monkeypatch.setattr(wsl2_usb, "AUTO_ATTACH_FILE", config_file)

    wsl2_usb._save_auto_attach_bus_ids({"2-1", "1-2"})

    assert json.loads(config_file.read_text()) == ["1-2", "2-1"]
    assert wsl2_usb._load_auto_attach_bus_ids() == {"1-2", "2-1"}


def test_load_auto_attach_bus_ids_ignores_invalid_config(monkeypatch, tmp_path):
    config_file = tmp_path / "auto_attach_busids.json"
    config_file.write_text("{invalid")
    monkeypatch.setattr(wsl2_usb, "AUTO_ATTACH_FILE", config_file)

    assert wsl2_usb._load_auto_attach_bus_ids() == set()


def test_main_runs_the_application(monkeypatch):
    calls = []
    fake_app = SimpleNamespace(run=lambda: calls.append("run"))
    monkeypatch.setattr(wsl2_usb, "Wsl2UsbApp", lambda: fake_app)

    wsl2_usb.main([])

    assert calls == ["run"]


def test_version_flag_prints_package_version(capsys):
    with pytest.raises(SystemExit) as exit_info:
        wsl2_usb.main(["--version"])

    assert exit_info.value.code == 0
    assert capsys.readouterr().out == "usbipd-tui 0.1.0\n"
