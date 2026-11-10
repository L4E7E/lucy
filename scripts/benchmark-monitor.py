#!/usr/bin/env python3
"""실행 중인 Lucy의 모니터 열기/닫기 부하 측정. 설정·전원 상태는 변경하지 않는다."""
import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--seconds", type=int, default=10)
parser.add_argument("--binary", type=Path, help="지정한 실행 파일의 입력 없는 독립 미리보기를 측정")
args = parser.parse_args()
if not 2 <= args.seconds <= 30:
    parser.error("--seconds must be between 2 and 30")
hz = os.sysconf("SC_CLK_TCK")


def telemetry():
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    pss = next(int(line.split()[1]) for line in
               (root / "smaps_rollup").read_text().splitlines() if line.startswith("Pss:"))
    return int(fields[11]) + int(fields[12]), pss / 1024


def stop_preview(child):
    if child.poll() is not None:
        return
    child.terminate()
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()


with tempfile.TemporaryDirectory(prefix="lucy-monitor-benchmark.") as directory, ExitStack() as cleanup:
    child = None
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    if args.binary:
        env = dict(os.environ)
        display = Path(env["WAYLAND_DISPLAY"])
        env.update(XDG_RUNTIME_DIR=directory,
                   WAYLAND_DISPLAY=str(display if display.is_absolute() else runtime / display),
                   PULSE_SERVER=f"unix:{runtime}/pulse/native", PIPEWIRE_RUNTIME_DIR=str(runtime),
                   LUCY_UI_TEST_NO_INPUT="1")
        child = subprocess.Popen([str(args.binary.resolve()), "bar", "--isolated", "--preview"],
                                 env=env, stdout=subprocess.DEVNULL)
        cleanup.callback(stop_preview, child)
        pid = child.pid
        server = str(Path(directory) / "lucy-bar-native-preview.sock")
    else:
        pid = int(subprocess.check_output(
            ["systemctl", "--user", "show", "lucy-bar.service", "-p", "MainPID", "--value"]))
        server = str(runtime / "lucy-bar.sock")
    root = Path(f"/proc/{pid}")
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    cleanup.callback(sock.close)
    sock.bind(directory + "/probe.sock")
    sock.settimeout(3)
    deadline = time.monotonic() + 6
    while not Path(server).exists():
        if time.monotonic() > deadline:
            raise RuntimeError("Lucy IPC가 준비되지 않았습니다.")
        time.sleep(.025)

    def send(command):
        sock.sendto(command.encode(), server)

    def status():
        send("@status")
        return json.loads(sock.recv(65536))

    initial = status()
    if initial["visible"] and initial["page"] != "system":
        raise SystemExit("다른 팝업을 사용 중입니다. 팝업을 닫은 뒤 다시 측정하세요.")

    def phase(name, opened):
        send("system" if opened else "@hide")
        deadline = time.monotonic() + 6
        while True:
            state = status()
            if (opened and state["ready"] and not state["expanding"]
                    and state["monitor"]["cpu"] is not None and state["size"][1] == 500
                    or not opened and not state["visible"]):
                break
            if time.monotonic() > deadline:
                raise RuntimeError("모니터 열기/닫기가 완료되지 않았습니다.")
            time.sleep(.025)
        time.sleep(.6)
        start = status()
        before, _ = telemetry()
        at = time.monotonic()
        memory = []
        for _ in range(args.seconds):
            time.sleep(1)
            current = status()
            if opened and (not current["visible"] or current["page"] != "system"
                           or current["surface"] != start["surface"]):
                raise RuntimeError("측정 중 팝업이 닫히거나 바뀌어 결과를 폐기합니다.")
            ticks, pss = telemetry()
            memory.append(pss)
        elapsed = time.monotonic() - at
        end = status()
        source = start["surface"] if opened else None

        def raster_count(state):
            return next((s["rasterized"] for s in state["surfaces"]
                         if s["surface"] == source), 0)

        maps = (root / "maps").read_text()
        cpu = 100 * (ticks - before) / hz / elapsed
        result = {"phase": name, "seconds": round(elapsed, 2),
                  "cpu_one_thread_percent": round(cpu, 2),
                  "cpu_machine_percent": round(cpu / os.cpu_count(), 3),
                  "pss_mib": round(statistics.mean(memory), 1),
                  "samples": end["monitor"]["sequence"] - start["monitor"]["sequence"],
                  "rasterized": raster_count(end) - raster_count(start),
                  "presented": ((end["presented"] - start["presented"]) if opened else 0),
                  "nvml_loaded": "libnvidia-ml.so" in maps,
                  "gpu_gauges": end.get("gpu_gauges")}
        if opened:
            result["output"] = next(b["output"] for b in initial["bars"]
                                    if b["surface"] == source)
        if not opened:
            assert result["samples"] == 0 and not result["nvml_loaded"]
        print(json.dumps(result, ensure_ascii=False), flush=True)

    try:
        phase("closed_before", False)
        phase("open", True)
        phase("closed_after", False)
    finally:
        send("system" if initial["visible"] else "@hide")
