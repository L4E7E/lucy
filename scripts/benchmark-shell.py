#!/usr/bin/env python3
"""입력 없는 읽기 전용 Lucy 미리보기의 CPU/RSS/PSS·프레임 비교.

기존 세션·설정·네트워크에는 쓰지 않는다. 같은 출력/조건에서 실행 파일을
바꿔 반복한다. 실행 중 컴파일이나 다른 부하를 겹치지 않는 것을 권장한다.
"""
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


def stop(child):
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


def run(binary, seconds, expected_output):
    with tempfile.TemporaryDirectory(prefix="lucy-shell-benchmark.") as directory, ExitStack() as cleanup:
        runtime = Path(os.environ["XDG_RUNTIME_DIR"])
        env = dict(os.environ)
        display = Path(env["WAYLAND_DISPLAY"])
        env.update(XDG_RUNTIME_DIR=directory,
                   WAYLAND_DISPLAY=str(display if display.is_absolute() else runtime / display),
                   PULSE_SERVER=f"unix:{runtime}/pulse/native", PIPEWIRE_RUNTIME_DIR=str(runtime),
                   LUCY_UI_TEST_NO_INPUT="1")
        child = subprocess.Popen([str(binary.resolve()), "bar", "--isolated", "--preview"],
                                 env=env, stdout=subprocess.DEVNULL)
        cleanup.callback(stop, child)
        root = Path(f"/proc/{child.pid}")
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        cleanup.callback(sock.close)
        sock.bind(directory + "/probe.sock")
        sock.settimeout(3)
        server = Path(directory) / "lucy-bar-native-preview.sock"
        deadline = time.monotonic() + 8
        while not server.exists():
            if child.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("Lucy preview failed to start")
            time.sleep(.025)

        def send(command):
            sock.sendto(command.encode(), str(server))

        def status():
            send("@status")
            return json.loads(sock.recv(262144))

        def counters():
            fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
            memory = dict((row.split()[0][:-1], int(row.split()[1]) / 1024)
                          for row in (root / "smaps_rollup").read_text().splitlines()[1:])
            return int(fields[11]) + int(fields[12]), memory

        def frames(state):
            return sum(w["presented"] for w in state["surfaces"])

        original_bars = {}

        def output_for(state):
            if not state["visible"]:
                return None
            if state["surface"] in original_bars:
                return original_bars[state["surface"]]
            if state["page"] == "launcher":
                layers = json.loads(subprocess.check_output(["niri", "msg", "-j", "layers"]))
                outputs = [w["output"] for w in layers if w["namespace"] == "lucy-launcher-popup"]
                if len(outputs) != 1:
                    raise RuntimeError("Cannot uniquely locate the preview launcher")
                return outputs[0]
            return None

        def measure(name, command, scroll=False):
            if command:
                send(command)
            time.sleep(.7)
            before = status()
            output = output_for(before)
            if output is not None and expected_output and output != expected_output:
                raise RuntimeError(f"Wrong output for {name}: {output} != {expected_output}")
            ticks, _ = counters()
            start = time.monotonic()
            samples = []
            next_scroll = 0
            while time.monotonic() - start < seconds:
                if scroll:
                    elapsed = time.monotonic() - start
                    if elapsed >= next_scroll:
                        # Alternate before reaching either edge; retain smooth wheel velocity.
                        send(f"@preview-scroll:{55 if int(elapsed / .8) % 2 == 0 else -55}")
                        next_scroll += .06
                time.sleep(.02 if scroll else .1)
                if len(samples) == 0 or time.monotonic() - start >= len(samples) * .25:
                    samples.append(counters()[1])
            elapsed = time.monotonic() - start
            end_ticks, _ = counters()
            after = status()
            if command != "@hide" and before["surface"] != after["surface"]:
                raise RuntimeError("Preview surface changed during measurement")
            cpu = 100 * (end_ticks - ticks) / os.sysconf("SC_CLK_TCK") / elapsed
            row = {"phase": name, "cpu_one_thread_percent": round(cpu, 2),
                   "cpu_machine_percent": round(cpu / os.cpu_count(), 3),
                   "rss_mib": round(statistics.mean(s["Rss"] for s in samples), 1),
                   "pss_mib": round(statistics.mean(s["Pss"] for s in samples), 1),
                   "private_mib": round(statistics.mean(s["Private_Clean"] + s["Private_Dirty"] for s in samples), 1),
                   "frames_per_second": round((frames(after) - frames(before)) / elapsed, 1)}
            if output:
                row["output"] = output
            if "allocator" in after:
                row.update({"heap_in_use_mib": round(after["allocator"]["in_use"] / 1048576, 1),
                            "heap_free_mib": round(after["allocator"]["free"] / 1048576, 1)})
            print(json.dumps(row, ensure_ascii=False), flush=True)

        time.sleep(2)
        original_bars.update({b["surface"]: b["output"] for b in status()["bars"]})
        measure("idle_cold", "@hide")
        for page in ["home", "audio", "network", "bluetooth", "power", "display", "calendar"]:
            measure(page, page)
        measure("launcher", "launcher")
        measure("launcher_scroll", None, True)
        send("@preview-calendar")
        send("@preview-expand-notices")
        measure("notifications_scroll", None, True)
        measure("idle_warm", "@hide")
        for _ in range(3):
            for page in ["home", "calendar", "launcher"]:
                send(page)
                time.sleep(.4)
                send("@hide")
                time.sleep(.4)
        measure("idle_after_cycles", "@hide")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--seconds", type=int, default=4)
    parser.add_argument("--expect-output", help="창이 지정한 모니터에 있는지 검증; 포커스/배율은 변경하지 않음")
    args = parser.parse_args()
    if not 2 <= args.seconds <= 30:
        parser.error("--seconds must be between 2 and 30")
    run(args.binary, args.seconds, args.expect_output)
