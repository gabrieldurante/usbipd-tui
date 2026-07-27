#!/usr/bin/env python3
"""WSL2 USB — Terminal UI for sharing USB devices with WSL2."""

import argparse
import json
import os
import re
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from rich.text import Text
from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.widgets import (
    Button, DataTable, Footer, Header, RichLog, Static,
)

__version__ = "0.1.0"

USBIPD = "usbipd.exe"
AUTO_REFRESH = 5.0
AUTO_SCAN_REFRESH = 0.5
AUTO_RETRY_DELAY = 30.0
CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "wsl2_usb"
AUTO_ATTACH_FILE = CONFIG_DIR / "auto_attach_busids.json"
# commands that require Windows administrator privileges
ADMIN_COMMANDS = frozenset({"bind", "unbind"})


# ── Data model ─────────────────────────────────────────────────────────────────

@dataclass
class Device:
    bus_id: Optional[str]
    client_ip: Optional[str]
    description: str
    instance_id: str
    is_forced: bool
    persisted_guid: Optional[str]
    stub_instance_id: Optional[str]

    @property
    def vid_pid(self) -> str:
        m = re.search(r"VID_([0-9A-Fa-f]+)&PID_([0-9A-Fa-f]+)", self.instance_id)
        return f"{m.group(1).upper()}:{m.group(2).upper()}" if m else "—"

    @property
    def state(self) -> str:
        if self.client_ip:
            return "Attached"
        if self.persisted_guid and self.bus_id:
            return "Bound"
        if self.persisted_guid:
            return "Persisted"
        return "Not Shared"

    @property
    def row_key(self) -> str:
        return self.bus_id or self.persisted_guid or self.instance_id

    def can_bind(self) -> bool:
        return self.state == "Not Shared"

    def can_unbind(self) -> bool:
        return self.state in ("Bound", "Persisted", "Attached")

    def can_attach(self) -> bool:
        return self.state == "Bound"

    def can_detach(self) -> bool:
        return self.state == "Attached"


STATE_STYLE = {
    "Attached": "bold green",
    "Bound": "yellow",
    "Persisted": "dim",
    "Not Shared": "",
}


# ── Helpers ────────────────────────────────────────────────────────────────────

def _fetch_devices() -> list[Device]:
    try:
        r = subprocess.run([USBIPD, "state"], capture_output=True, text=True, timeout=10)
        return [
            Device(
                bus_id=d.get("BusId"),
                client_ip=d.get("ClientIPAddress"),
                description=d.get("Description", ""),
                instance_id=d.get("InstanceId", ""),
                is_forced=d.get("IsForced", False),
                persisted_guid=d.get("PersistedGuid"),
                stub_instance_id=d.get("StubInstanceId"),
            )
            for d in json.loads(r.stdout).get("Devices", [])
        ]
    except Exception:
        return []



def _run_usbipd(*args: str) -> tuple[int, str, str]:
    if args and args[0] in ADMIN_COMMANDS:
        return _run_elevated(*args)
    try:
        r = subprocess.run([USBIPD, *args], capture_output=True, text=True, timeout=30)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return 1, "", "Command timed out"
    except Exception as e:
        return 1, "", str(e)


def _load_auto_attach_bus_ids() -> set[str]:
    try:
        data = json.loads(AUTO_ATTACH_FILE.read_text())
        if isinstance(data, list):
            return {str(item) for item in data if item}
    except (OSError, json.JSONDecodeError):
        pass
    return set()


def _save_auto_attach_bus_ids(bus_ids: set[str]) -> None:
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    AUTO_ATTACH_FILE.write_text(json.dumps(sorted(bus_ids), indent=2) + "\n")


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _ps_arg_quote(value: str) -> str:
    return '"' + value.replace('"', '\\"') + '"'


def _admin_effect_applied(args: tuple[str, ...]) -> bool:
    devices = _fetch_devices()
    if not devices:
        return False

    command = args[0] if args else ""
    if command == "bind":
        if "--busid" in args:
            bus_id = args[args.index("--busid") + 1]
            return any(d.bus_id == bus_id and d.persisted_guid for d in devices)
        if "--hardware-id" in args:
            hardware_id = args[args.index("--hardware-id") + 1]
            return any(d.vid_pid == hardware_id and d.persisted_guid for d in devices)

    if command == "unbind":
        if "--all" in args:
            return all(not d.persisted_guid for d in devices)
        if "--busid" in args:
            bus_id = args[args.index("--busid") + 1]
            return all(d.bus_id != bus_id or not d.persisted_guid for d in devices)
        if "--guid" in args:
            guid = args[args.index("--guid") + 1]
            return all(d.persisted_guid != guid for d in devices)

    return False


def _run_elevated(*args: str) -> tuple[int, str, str]:
    """Run usbipd elevated via a temp PS1 + Start-Process -Verb RunAs, capturing output through temp files."""
    uid = uuid.uuid4().hex[:8]
    win_tmp = "C:\\Windows\\Temp"
    wsl_tmp = "/mnt/c/Windows/Temp"
    base = f"wsl2_usb_{uid}"

    def wp(ext: str) -> str: return f"{win_tmp}\\{base}.{ext}"  # Windows path
    def lp(ext: str) -> str: return f"{wsl_tmp}/{base}.{ext}"   # Linux/WSL path

    # Inner PS1: this script itself is elevated, so run usbipd directly and
    # persist the exit code for the WSL caller to read after UAC returns.
    ps_args = ", ".join(_ps_quote(a) for a in args)
    script = (
        "$ErrorActionPreference = 'Continue'\n"
        f"$usbipdArgs = @({ps_args})\n"
        f"& {_ps_quote(USBIPD)} @usbipdArgs"
        f" > {_ps_quote(wp('out'))} 2> {_ps_quote(wp('err'))}\n"
        "$rc = if ($LASTEXITCODE -is [int]) { $LASTEXITCODE } else { 1 }\n"
        f"$rc | Out-File -FilePath {_ps_quote(wp('rc'))} -Encoding ASCII -NoNewline\n"
    )
    try:
        with open(lp("ps1"), "w") as f:
            f.write(script)
    except OSError as e:
        return 1, "", f"Could not write temp script: {e}"

    ps1_win = wp("ps1")
    outer_args = " ".join(
        [
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            _ps_arg_quote(ps1_win),
        ]
    )
    try:
        elevated = subprocess.run(
            [
                "powershell.exe", "-NoProfile", "-WindowStyle", "Hidden", "-Command",
                f"Start-Process powershell.exe"
                f" -ArgumentList {_ps_quote(outer_args)}"
                f" -Verb RunAs -Wait",
            ],
            capture_output=True,
            text=True,
            timeout=90,
        )
        if elevated.returncode != 0:
            _cleanup(lp("ps1"), lp("out"), lp("err"), lp("rc"))
            detail = (elevated.stderr or elevated.stdout).strip()
            return 1, "", detail or f"Elevation launcher failed with exit {elevated.returncode}"
    except subprocess.TimeoutExpired:
        _cleanup(lp("ps1"), lp("out"), lp("err"), lp("rc"))
        return 1, "", "Timed out waiting for elevated process"
    except Exception as e:
        _cleanup(lp("ps1"), lp("out"), lp("err"), lp("rc"))
        return 1, "", f"Elevation failed: {e}"

    rc_path = lp("rc")
    deadline = time.monotonic() + 10
    while not os.path.exists(rc_path) and time.monotonic() < deadline:
        time.sleep(0.2)

    out, err, rc = "", "", 1
    try:
        out = open(lp("out"), encoding="utf-8-sig").read().strip()
    except OSError:
        pass
    try:
        err = open(lp("err"), encoding="utf-8-sig").read().strip()
    except OSError:
        pass
    try:
        rc = int(open(rc_path).read().strip())
    except (OSError, ValueError):
        if _admin_effect_applied(args):
            rc = 0
        else:
            rc = 1
            if not err:
                err = "Elevated process did not return a status. UAC may have been cancelled or the temp files were not written."

    _cleanup(lp("ps1"), lp("out"), lp("err"), lp("rc"))
    return rc, out, err


def _cleanup(*paths: str) -> None:
    for p in paths:
        try:
            os.unlink(p)
        except OSError:
            pass


# ── Main app ───────────────────────────────────────────────────────────────────

class Wsl2UsbApp(App):
    TITLE = "WSL2 USB"
    SUB_TITLE = "USB device sharing for WSL2"
    CSS_PATH = None

    BINDINGS = [
        Binding("r", "refresh",          "Refresh",        show=True),
        Binding("b", "bind",             "Bind",           show=True),
        Binding("a", "attach",           "Attach WSL2",    show=True),
        Binding("d", "detach",           "Detach",         show=True),
        Binding("u", "unbind",           "Unbind",         show=True),
        Binding("D", "detach_all",       "Detach All",     show=True),
        Binding("U", "unbind_all",       "Unbind All",     show=True),
        Binding("x", "toggle_auto_attach", "Auto Bus",     show=True),
        Binding("p", "toggle_persisted", "Toggle Persisted", show=True),
        Binding("q", "quit",             "Quit",           show=True),
    ]

    CSS = """
    Screen { layout: vertical; }

    #table-pane {
        height: 1fr;
        border: round $primary;
        margin: 0 1;
    }
    #device-table { height: 1fr; }

    #status-line {
        height: 1;
        margin: 0 1;
        color: $text-muted;
    }

    #action-row {
        height: auto;
        padding: 0 1;
        margin: 0 0 1 0;
    }
    #action-row Button { margin-right: 1; }
    #action-row Button:disabled { opacity: 40%; }

    #log-pane {
        height: 9;
        border: round $accent;
        margin: 0 1;
    }
    #log { height: 1fr; padding: 0 1; }

    """

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Vertical(id="table-pane"):
            yield DataTable(id="device-table", cursor_type="row")
        yield Static("Loading devices...", id="status-line")
        with Horizontal(id="action-row"):
            yield Button("Bind",        id="btn-bind",        variant="primary",  disabled=True)
            yield Button("Attach WSL2", id="btn-attach",      variant="default",  disabled=True)
            yield Button("Detach",      id="btn-detach",      variant="warning",  disabled=True)
            yield Button("Unbind",      id="btn-unbind",      variant="error",    disabled=True)
            yield Button("Auto Bus",    id="btn-auto",        variant="default",  disabled=True)
            yield Button("Detach All",  id="btn-detach-all",  variant="warning")
            yield Button("Unbind All",  id="btn-unbind-all",  variant="error")
            yield Button("Refresh",     id="btn-refresh",     variant="default")
        with Vertical(id="log-pane"):
            yield RichLog(id="log", highlight=True, markup=True)
        yield Footer()

    def on_mount(self) -> None:
        self._devices: dict[str, Device] = {}
        self._selected_key: Optional[str] = None
        self._show_persisted: bool = True
        self._last_devices: list[Device] = []
        self._auto_attach_bus_ids: set[str] = _load_auto_attach_bus_ids()
        self._auto_inflight: set[str] = set()
        self._auto_scan_inflight: bool = False
        self._auto_last_attempt: dict[str, float] = {}
        t = self.query_one("#device-table", DataTable)
        t.add_columns("BUS ID", "VID:PID", "DESCRIPTION", "STATE", "CLIENT", "F", "A")
        self._log("[dim]Loading devices…[/dim]")
        if self._auto_attach_bus_ids:
            self._log(
                "[dim]Auto bus-id enabled: "
                + ", ".join(sorted(self._auto_attach_bus_ids))
                + "[/dim]"
            )
        self._do_refresh()
        self.set_interval(AUTO_REFRESH, self._do_refresh)
        self.set_interval(AUTO_SCAN_REFRESH, self._do_auto_scan)

    # ── Refresh ────────────────────────────────────────────────────────────

    def _do_refresh(self) -> None:
        self._refresh_worker()

    @work(thread=True)
    def _refresh_worker(self) -> None:
        devices = _fetch_devices()
        self.call_from_thread(self._update_table, devices)

    def _do_auto_scan(self) -> None:
        if not self._auto_attach_bus_ids or self._auto_scan_inflight:
            return
        self._auto_scan_inflight = True
        self._auto_scan_worker()

    @work(thread=True)
    def _auto_scan_worker(self) -> None:
        devices = _fetch_devices()
        connected = [d for d in devices if d.bus_id is not None]
        self.call_from_thread(self._finish_auto_scan, connected)

    def _finish_auto_scan(self, devices: list[Device]) -> None:
        self._auto_scan_inflight = False
        self._maybe_auto_attach(devices)

    def _update_table(self, devices: list[Device]) -> None:
        self._last_devices = devices

        # connected devices first (BusId set), then persisted-only (BusId null)
        connected   = [d for d in devices if d.bus_id is not None]
        persisted   = [d for d in devices if d.bus_id is None]
        visible     = connected + (persisted if self._show_persisted else [])

        t = self.query_one("#device-table", DataTable)
        t.clear()
        self._devices = {}
        cursor_row = 0
        for i, d in enumerate(visible):
            state_style = STATE_STYLE.get(d.state, "")
            self._devices[d.row_key] = d
            t.add_row(
                d.bus_id or "—",
                d.vid_pid,
                d.description[:46],
                Text(d.state, style=state_style),
                d.client_ip or "—",
                "F" if d.is_forced else "",
                "A" if d.bus_id in self._auto_attach_bus_ids else "",
                key=d.row_key,
            )
            if d.row_key == self._selected_key:
                cursor_row = i
        if t.row_count > 0:
            t.move_cursor(row=cursor_row)
        self._update_buttons()

        # subtitle: live device counts + persisted toggle hint
        p_label = f"{len(persisted)} persisted"
        if not self._show_persisted and persisted:
            p_label += " [hidden, p to show]"
        self.sub_title = f"{len(connected)} connected  ·  {p_label}"
        if visible:
            self.query_one("#status-line", Static).update(
                "Use the highlighted row with the footer shortcuts or buttons."
            )
        elif devices:
            self.query_one("#status-line", Static).update(
                "Only persisted devices were found. Press p to show them."
            )
        else:
            self.query_one("#status-line", Static).update("No USB devices found.")
        self._maybe_auto_attach(connected)

    # ── Selection & button state ───────────────────────────────────────────

    def on_data_table_row_highlighted(self, event: DataTable.RowHighlighted) -> None:
        self._selected_key = str(event.row_key.value) if event.row_key else None
        self._update_buttons()

    def _selected(self) -> Optional[Device]:
        return self._devices.get(self._selected_key) if self._selected_key else None

    def _update_buttons(self) -> None:
        dev = self._selected()
        self.query_one("#btn-bind",   Button).disabled = not (dev and dev.can_bind())
        self.query_one("#btn-attach", Button).disabled = not (dev and dev.can_attach())
        self.query_one("#btn-detach", Button).disabled = not (dev and dev.can_detach())
        self.query_one("#btn-unbind", Button).disabled = not (dev and dev.can_unbind())
        self.query_one("#btn-auto",   Button).disabled = not (dev and dev.bus_id)

    # ── Command runner ─────────────────────────────────────────────────────

    def _log(self, msg: str) -> None:
        self.query_one("#log", RichLog).write(msg)

    @work(thread=True)
    def _run(self, *args: str) -> None:
        self.call_from_thread(
            self._log, "[dim]$ " + " ".join([USBIPD, *args]) + "[/dim]"
        )
        if args and args[0] in ADMIN_COMMANDS:
            self.call_from_thread(
                self._log, "[yellow]⚡ Requires elevation — Windows UAC prompt will appear[/yellow]"
            )
        rc, out, err = _run_usbipd(*args)
        if out.strip():
            self.call_from_thread(self._log, out.strip())
        if err.strip():
            self.call_from_thread(self._log, f"[red]{err.strip()}[/red]")
        self.call_from_thread(
            self._log,
            "[bold green]✓ OK[/bold green]" if rc == 0 else f"[bold red]✗ Failed (exit {rc})[/bold red]",
        )
        self.call_from_thread(self._do_refresh)

    def _maybe_auto_attach(self, devices: list[Device]) -> None:
        now = time.monotonic()
        for dev in devices:
            if not dev.bus_id or dev.bus_id not in self._auto_attach_bus_ids:
                continue
            if dev.client_ip or dev.bus_id in self._auto_inflight:
                continue
            last_attempt = self._auto_last_attempt.get(dev.bus_id, 0)
            if now - last_attempt < AUTO_RETRY_DELAY:
                continue
            self._auto_last_attempt[dev.bus_id] = now
            self._auto_inflight.add(dev.bus_id)
            self._auto_attach_worker(dev.bus_id, dev.state)

    @work(thread=True)
    def _auto_attach_worker(self, bus_id: str, state: str) -> None:
        self._auto_inflight.add(bus_id)
        try:
            self.call_from_thread(
                self._log, f"[dim]Auto bus-id {bus_id}: detected {state} device[/dim]"
            )
            if state == "Not Shared":
                self.call_from_thread(
                    self._log,
                    f"[dim]$ {USBIPD} bind --busid {bus_id}[/dim]",
                )
                self.call_from_thread(
                    self._log,
                    "[yellow]Auto bind requires elevation - Windows UAC prompt will appear[/yellow]",
                )
                rc, out, err = _run_usbipd("bind", "--busid", bus_id)
                if out.strip():
                    self.call_from_thread(self._log, out.strip())
                if err.strip():
                    self.call_from_thread(self._log, f"[red]{err.strip()}[/red]")
                if rc != 0:
                    self.call_from_thread(
                        self._log,
                        f"[bold red]Auto bus-id {bus_id}: bind failed (exit {rc})[/bold red]",
                    )
                    return

            self.call_from_thread(
                self._log,
                f"[dim]$ {USBIPD} attach --busid {bus_id} --wsl[/dim]",
            )
            rc, out, err = _run_usbipd("attach", "--busid", bus_id, "--wsl")
            if out.strip():
                self.call_from_thread(self._log, out.strip())
            if err.strip():
                self.call_from_thread(self._log, f"[red]{err.strip()}[/red]")
            self.call_from_thread(
                self._log,
                f"[bold green]Auto bus-id {bus_id}: attached[/bold green]"
                if rc == 0
                else f"[bold red]Auto bus-id {bus_id}: attach failed (exit {rc})[/bold red]",
            )
        finally:
            self._auto_inflight.discard(bus_id)
            self.call_from_thread(self._do_refresh)

    # ── Actions ────────────────────────────────────────────────────────────

    def action_refresh(self) -> None:
        self._log("[dim]Refreshing…[/dim]")
        self._do_refresh()

    def action_toggle_persisted(self) -> None:
        self._show_persisted = not self._show_persisted
        # re-render from last known data without a new network call
        if self._last_devices:
            self._update_table(self._last_devices)
        label = "shown" if self._show_persisted else "hidden"
        self._log(f"[dim]Persisted (disconnected) devices: {label}[/dim]")

    def action_toggle_auto_attach(self) -> None:
        dev = self._selected()
        if not (dev and dev.bus_id):
            return
        if dev.bus_id in self._auto_attach_bus_ids:
            self._auto_attach_bus_ids.remove(dev.bus_id)
            self._auto_inflight.discard(dev.bus_id)
            self._log(f"[dim]Auto bus-id disabled: {dev.bus_id}[/dim]")
        else:
            self._auto_attach_bus_ids.add(dev.bus_id)
            self._auto_last_attempt.pop(dev.bus_id, None)
            self._log(f"[dim]Auto bus-id enabled: {dev.bus_id}[/dim]")
        try:
            _save_auto_attach_bus_ids(self._auto_attach_bus_ids)
        except OSError as e:
            self._log(f"[red]Could not save auto bus-id rules: {e}[/red]")
        if self._last_devices:
            self._update_table(self._last_devices)

    def action_bind(self) -> None:
        dev = self._selected()
        if not (dev and dev.can_bind()):
            return
        if dev.bus_id:
            self._run("bind", "--busid", dev.bus_id)
        else:
            self._run("bind", "--hardware-id", dev.vid_pid)

    def action_unbind(self) -> None:
        dev = self._selected()
        if not (dev and dev.can_unbind()):
            return
        if dev.bus_id:
            self._run("unbind", "--busid", dev.bus_id)
        elif dev.persisted_guid:
            self._run("unbind", "--guid", dev.persisted_guid)

    def action_attach(self) -> None:
        dev = self._selected()
        if dev and dev.can_attach():
            self._run("attach", "--busid", dev.bus_id, "--wsl")

    def action_detach(self) -> None:
        dev = self._selected()
        if dev and dev.can_detach():
            self._run("detach", "--busid", dev.bus_id)

    def action_detach_all(self) -> None:
        self._run("detach", "--all")

    def action_unbind_all(self) -> None:
        self._run("unbind", "--all")

    # ── Button → action wiring ─────────────────────────────────────────────

    @on(Button.Pressed, "#btn-bind")
    def _btn_bind(self):        self.action_bind()
    @on(Button.Pressed, "#btn-attach")
    def _btn_attach(self):      self.action_attach()
    @on(Button.Pressed, "#btn-detach")
    def _btn_detach(self):      self.action_detach()
    @on(Button.Pressed, "#btn-unbind")
    def _btn_unbind(self):      self.action_unbind()
    @on(Button.Pressed, "#btn-auto")
    def _btn_auto(self):        self.action_toggle_auto_attach()
    @on(Button.Pressed, "#btn-detach-all")
    def _btn_detach_all(self):  self.action_detach_all()
    @on(Button.Pressed, "#btn-unbind-all")
    def _btn_unbind_all(self):  self.action_unbind_all()
    @on(Button.Pressed, "#btn-refresh")
    def _btn_refresh(self):     self.action_refresh()


def main(argv: Optional[list[str]] = None) -> None:
    parser = argparse.ArgumentParser(prog="usbipd-tui", description=__doc__)
    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s {__version__}",
    )
    parser.parse_args(argv)
    Wsl2UsbApp().run()


if __name__ == "__main__":
    main()
