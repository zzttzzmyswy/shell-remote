#!/usr/bin/env python3
"""Stop recorded test PIDs and their descendants, collecting before termination."""
import os
import signal
import sys
import time
from pathlib import Path


def processes():
    result = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            result[int(path.parent.name)] = (int(fields[1]), fields[19], fields[0])
        except (OSError, ValueError, IndexError):
            pass
    return result


def stop(roots):
    snapshot = processes()
    owned = set(roots) & snapshot.keys()
    while True:
        children = {pid for pid, data in snapshot.items() if data[0] in owned}
        if children <= owned:
            break
        owned |= children
    owned.discard(os.getpid())
    identities = {pid: snapshot[pid][1] for pid in owned}

    def live():
        now = processes()
        return [pid for pid, birth in identities.items()
                if pid in now and now[pid][1] == birth and now[pid][2] != 'Z']

    for sig, seconds in ((signal.SIGTERM, 2), (signal.SIGKILL, 5)):
        for pid in live():
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + seconds
        while live() and time.monotonic() < deadline:
            time.sleep(0.05)
    remaining = live()
    if remaining:
        raise RuntimeError(f'test processes still running: {remaining}')


if __name__ == '__main__':
    stop([int(pid) for pid in sys.argv[1:] if pid])
