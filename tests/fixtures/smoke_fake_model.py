"""Offline protocol smoke only; does not establish real CLI compatibility."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request


def main():
    with tempfile.TemporaryDirectory(prefix='connecto-fake-model-') as root:
        root = pathlib.Path(root)
        command = root / 'command.txt'
        command.write_text('printf harmless-fixture')
        log = root / 'metadata.jsonl'
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        proc = subprocess.Popen([sys.executable, str(pathlib.Path(__file__).with_name('fake_model.py')),
                                 str(port), str(log), str(command)], stdout=subprocess.DEVNULL,
                                stderr=subprocess.PIPE)
        try:
            base = f'http://127.0.0.1:{port}'
            for _ in range(100):
                try:
                    with urllib.request.urlopen(base, timeout=0.1) as response:
                        assert response.status == 200
                    break
                except OSError:
                    if proc.poll() is not None:
                        raise RuntimeError('Fixture exited during startup')
                    time.sleep(0.02)
            else:
                raise RuntimeError('Fixture startup deadline exceeded')
            cases = [
                ('/v1/messages', {'messages': [{'role': 'user', 'content': 'synthetic-marker'}]}, 'FAKE-CLAUDE-REPLY'),
                ('/v1/responses', {'input': [{'role': 'user', 'content': 'synthetic-marker'}]}, 'FAKE-CODEX-REPLY'),
                ('/v1/messages', {'messages': [{'role': 'user', 'content': 'RUNTOOL'}]}, 'tool_use'),
                ('/v1/responses', {'input': [{'role': 'user', 'content': 'RUNTOOL'}], 'tools': [{'name': 'exec_command'}]}, 'function_call'),
            ]
            for endpoint, payload, expected in cases:
                request = urllib.request.Request(base + endpoint, json.dumps(payload).encode(),
                                                 {'Content-Type': 'application/json'})
                with urllib.request.urlopen(request, timeout=2) as response:
                    assert expected in response.read().decode(), endpoint
            records = log.read_text()
            assert 'synthetic-marker' not in records
            assert 'harmless-fixture' not in records
            assert all(set(json.loads(line)) == {'family', 'bytes', 'sha256'} for line in records.splitlines())
            print('PASS: 4 HTTP/SSE scenarios; metadata-only logging. UNIT tooling smoke, no CLI/model turns.')
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
            proc.stderr.close()


if __name__ == '__main__':
    main()
