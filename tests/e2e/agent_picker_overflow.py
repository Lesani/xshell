#!/usr/bin/env python3
"""Native release regression for AgentPickerDialog (no Vite or injected UI state).

Run the published binary with --expect-layout fail, then the reviewed release with
--expect-layout pass, retaining the same isolated HOME. Evidence is private JSON,
WebDriver operations and PNGs. Setup/interaction errors never count as layout RED.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import time
import traceback
import urllib.error
import urllib.request

LABELS = ["Claude Code", "Codex", "Cursor", "opencode", "Antigravity", "Pi", "Shell"]
BINARIES = ["claude", "codex", "cursor-agent", "opencode", "agy", "pi"]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"
PROJECT_NAME = "ProjectWithAVeryLongUnbrokenNameForPickerOverflowVerification0123456789"
TAB, SHIFT, ENTER, SPACE, ESCAPE = "\ue004", "\ue008", "\ue007", " ", "\ue00c"

# Read-only: use Range rectangles as well as scrollWidth so clipping cannot hide RED.
GEOMETRY = r"""
const p=document.querySelector('.agent-picker');
if (!p) return null;
const rect=e=>e.getBoundingClientRect().toJSON();
const ranges=e=>{const out=[],walk=document.createTreeWalker(e,NodeFilter.SHOW_TEXT);
 while(walk.nextNode()) { if(!walk.currentNode.textContent.trim()) continue;
  const r=document.createRange();r.selectNodeContents(walk.currentNode);
  out.push(...[...r.getClientRects()].map(x=>x.toJSON())); } return out;};
const info=e=>({rect:rect(e),clientWidth:e.clientWidth,scrollWidth:e.scrollWidth,
 clientHeight:e.clientHeight,scrollHeight:e.scrollHeight,
 overflowX:getComputedStyle(e).overflowX,overflowY:getComputedStyle(e).overflowY});
return {viewport:{width:innerWidth,height:innerHeight},dialog:info(p),
 options:info(p.querySelector('.agent-picker-options')),scrollTop:p.scrollTop,
 sections:[...p.children].map(e=>({className:e.className,...info(e)})),
 texts:[...p.querySelectorAll('.picker-title,.picker-sub,.picker-footer-hint')]
  .map(e=>({text:e.innerText,rect:rect(e),ranges:ranges(e)})),
 buttons:[...p.querySelectorAll('button')].map(e=>({label:e.innerText.trim(),
  ...info(e),textRanges:ranges(e),
  hit:e.contains(document.elementFromPoint(rect(e).x+rect(e).width/2,
    rect(e).y+rect(e).height/2))})),
 focus:{label:document.activeElement.innerText?.trim(),
  option:document.activeElement.matches('.agent-picker-option'),
  settings:document.activeElement.matches('.agent-picker .picker-footer button')}};
"""


def digest(path):
    with open(path, "rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def fixture(home):
    """Seed only a new private HOME; retained runs must never reset settings/data."""
    marker = home / "x74-fixture.json"
    if marker.exists():
        return json.loads(marker.read_text())
    if home.exists() and any(home.iterdir()):
        raise RuntimeError("Refusing an existing non-fixture HOME")
    home.mkdir(parents=True, exist_ok=True, mode=0o700)
    home.chmod(0o700)
    bindir = home / "fixture-bin"
    bindir.mkdir()
    path = f"{bindir}:{Path(shutil.which('tauri-driver')).parent}:/usr/local/bin:/usr/bin:/bin"
    script = '''#!/usr/bin/python3
import os,sys,time,json
from pathlib import Path
if '--version' in sys.argv:
 print('x74-standin 1.4.0');sys.exit()
r={'binary':Path(sys.argv[0]).name,'argv':sys.argv[1:],'cwd':os.getcwd(),
   'pid':os.getpid(),'terminal':os.environ.get('XSHELL_TERMINAL_ID')}
with open(Path.home()/'fixture-launches.jsonl','a') as f: f.write(json.dumps(r)+'\\n')
print('X74_FIXTURE '+json.dumps(r),flush=True)
while True: time.sleep(1)
'''
    for name in BINARIES:
        p = bindir / name
        p.write_text(script)
        p.chmod(0o700)
    shell_log = '''
if [ -n "$XSHELL_TERMINAL_ID" ]; then
/usr/bin/python3 -c 'import os,json;from pathlib import Path;r={"binary":"bash","cwd":os.getcwd(),"pid":os.getppid(),"terminal":os.environ["XSHELL_TERMINAL_ID"]};f=open(Path.home()/"fixture-launches.jsonl","a");f.write(json.dumps(r)+"\\n");f.close();print("X74_SHELL "+json.dumps(r))'
fi
'''
    for name in [".bashrc", ".bash_profile", ".profile"]:
        (home / name).write_text(f"export PATH={path}\n" + (shell_log if name == ".bashrc" else ""))
    project = home / PROJECT_NAME
    project.mkdir()
    settings = {"project_paths": [str(project)], "default_agent": "ask",
                "theme": "dark", "default_shell": "bash"}
    store = home / ".local/share/com.xshell.app/settings.json"
    store.parent.mkdir(parents=True)
    store.write_text(json.dumps(settings))
    result = {"home": str(home), "path": path, "project": str(project),
              "settingsSeed": settings, "rcFiles": [".bashrc", ".bash_profile", ".profile"]}
    marker.write_text(json.dumps(result, indent=2))
    return result


class Runner:
    def __init__(self, args):
        self.a = args
        self.home = args.home.resolve()
        self.output = args.output.resolve()
        self.output.mkdir(parents=True, exist_ok=False)
        self.operations = (self.output / "webdriver.jsonl").open("w")
        self.state_path = Path(f"/tmp/desktop-e2e-{os.getuid()}-{args.port}.json")
        self.sid = None
        self.started = False
        self.start_attempted = False
        self.report = {"expectLayout": args.expect_layout, "home": str(self.home),
                       "port": args.port, "samples": [], "interactions": [], "status": "setup-failure"}

    def request(self, method, suffix, body=None):
        path = f"/session/{self.sid}{suffix}"
        entry = {"method": method, "path": path, "body": body}
        req = urllib.request.Request(f"http://127.0.0.1:{self.a.port}{path}", method=method,
                                     headers={"Content-Type": "application/json"},
                                     data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(req, timeout=120) as f:
                result = json.load(f)["value"]
            entry["result"] = "ok"
            return result
        except urllib.error.HTTPError as e:
            entry["error"] = e.read().decode()
            raise RuntimeError(entry["error"]) from e
        finally:
            self.operations.write(json.dumps(entry) + "\n")
            self.operations.flush()

    def js(self, script, *args):
        return self.request("POST", "/execute/sync", {"script": script, "args": list(args)})

    def wait(self, predicate, description):
        end = time.monotonic() + 25
        while time.monotonic() < end:
            value = predicate()
            if value:
                return value
            time.sleep(.15)
        raise RuntimeError(f"Timeout: {description}")

    def exists(self, css):
        return self.js("return !!document.querySelector(arguments[0])", css)

    def click(self, css, text=None):
        # Obtaining an element is observational; activation uses native WebDriver click.
        element = self.js("""return [...document.querySelectorAll(arguments[0])].find(e=>
          e.offsetParent!==null && (arguments[1]===null || e.innerText.trim()===arguments[1]))""", css, text)
        if not element:
            raise RuntimeError(f"No visible native click target {css}: {text}")
        self.request("POST", f"/element/{element[ELEMENT]}/click", {})
        time.sleep(.25)

    def key(self, key, shift=False):
        actions = ([{"type": "keyDown", "value": SHIFT}] if shift else [])
        actions += [{"type": "keyDown", "value": key}, {"type": "keyUp", "value": key}]
        if shift:
            actions += [{"type": "keyUp", "value": SHIFT}]
        self.request("POST", "/actions", {"actions": [{"type": "key", "id": "keyboard", "actions": actions}]})
        time.sleep(.12)

    def wheel(self, delta):
        g = self.js(GEOMETRY)
        r = g["dialog"]["rect"]
        self.request("POST", "/actions", {"actions": [{"type": "wheel", "id": "wheel",
            "actions": [{"type": "scroll", "x": int(r["x"] + r["width"] / 2),
                         "y": int(r["y"] + r["height"] / 2), "deltaX": 0,
                         "deltaY": delta, "duration": 150}]}]})
        time.sleep(.2)

    def shot(self, name):
        data = self.request("GET", "/screenshot")
        (self.output / f"{name}.png").write_bytes(base64.b64decode(data))

    def driver(self, *args):
        command = [str(self.a.driver), *map(str, args), "--port", str(self.a.port)]
        with (self.output / "driver-commands.log").open("a") as log:
            log.write(json.dumps(command) + "\n")
            log.flush()
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)

    def start(self):
        if self.state_path.exists():
            raise RuntimeError("Port has existing driver state; refusing to take ownership")
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", self.a.port)) == 0:
                raise RuntimeError("Port already has a listener; refusing to start")
        self.f = fixture(self.home)
        binary = self.a.binary.resolve(strict=True)
        daemons = sorted(p for p in binary.parent.glob("xshelld-*") if not p.name.endswith(".d"))
        daemon = daemons[0] if daemons else binary.parent / "xshelld"
        if not daemon.is_file():
            raise RuntimeError("Missing matching release sidecar")
        self.report["fixture"] = self.f
        self.report["artifact"] = {"binary": str(binary), "daemon": str(daemon),
            "sha256": {p.name: digest(p) for p in [binary, daemon]},
            "manifest": (binary.parent / "manifest").read_text() if (binary.parent / "manifest").exists() else None}
        self.start_attempted = True
        self.driver("start", binary, self.home, "--env", f"PATH={self.f['path']}", "--env", "SHELL=/bin/bash")
        self.started = True
        state = json.loads(self.state_path.read_text())
        self.sid = state["sid"]
        self.report["driverState"] = state
        assert digest(self.home / ".xshell/bin/xshell") == digest(binary)
        assert digest(self.home / ".xshell/bin/xshelld") == digest(daemon)
        self.wait(lambda: self.exists('.tb-settings-btn'), "release startup")
        self.resize(1280, 800)
        self.click('.tb-settings-btn')
        self.click('.settings-nav-item', 'Agents')
        text = self.wait(lambda: self.js("return document.querySelector('.settings-body')?.innerText"), "agent probes")
        self.wait(lambda: self.js("return document.querySelectorAll('.settings-agent-path').length") == 6, "six installed fixtures")
        text = self.js("return document.querySelector('.settings-body').innerText")
        for name in BINARIES:
            assert str(self.home / "fixture-bin" / name) in text, f"Real provider or missing probe: {name}"
        assert self.js("return document.querySelector('.settings-body select').value") == "ask"
        (self.output / "settings-probes.txt").write_text(text)
        self.shot("settings-six-fixtures")
        while self.terminal_count():
            self.click('.tab-item .tab-item-close')
            time.sleep(.3)

    def stop(self):
        if not self.start_attempted:
            return
        stop_error = None
        if self.state_path.exists():
            state = json.loads(self.state_path.read_text())
            if state['home'] != str(self.home):
                raise RuntimeError('Refusing to stop another profile')
            try:
                self.driver("stop")
            except (subprocess.SubprocessError, OSError) as e:
                stop_error = str(e)
            self.started = False
        # start can fail before desktop-e2e writes its state. Its children all
        # inherit this dedicated fixture HOME; terminate only those exact PIDs.
        for proc in Path('/proc').iterdir():
            if not proc.name.isdigit():
                continue
            try:
                env = (proc / 'environ').read_bytes().split(b'\0')
                if f'HOME={self.home}'.encode() in env:
                    os.kill(int(proc.name), signal.SIGTERM)
            except (OSError, PermissionError):
                pass
        self.start_attempted = False
        log = Path(f"/tmp/desktop-e2e-{os.getuid()}-{self.a.port}.log")
        if log.exists():
            shutil.copy2(log, self.output / "native-driver.log")
        # Report only our profile's processes, never kill by broad name matching.
        time.sleep(1)
        remaining = []
        for proc in Path('/proc').iterdir():
            if not proc.name.isdigit():
                continue
            try:
                exe = (proc / 'exe').resolve()
                cmd = (proc / 'cmdline').read_bytes().replace(b'\0', b' ').decode()
                if str(self.home) in str(exe) or str(self.home / 'fixture-bin') in cmd:
                    remaining.append({"pid": proc.name, "command": cmd})
            except (OSError, UnicodeError):
                pass
        self.report["cleanup"] = {"driverStateRemoved": not self.state_path.exists(),
                                  "remainingOwnedProcesses": remaining, "stopError": stop_error}
        if remaining or self.state_path.exists() or stop_error:
            self.report['status'] = 'cleanup-failure'
            raise AssertionError(f"Incomplete cleanup: {self.report['cleanup']}")

    def resize(self, width, height):
        actual = self.request("POST", "/window/rect", {"width": width, "height": height})
        time.sleep(.3)
        viewport = self.js("return {width:innerWidth,height:innerHeight}")
        assert actual["width"] == width and actual["height"] == height, actual
        assert viewport == {"width": width, "height": height}, viewport
        self.rect = {"requested": {"width": width, "height": height}, "native": actual, "viewport": viewport}

    def open(self, entry):
        if entry == "home":
            self.click('.ds-item[data-drop-kind="project"]')
            self.click('button.btn-primary', 'New Chat')
        elif entry == "tabbar":
            if not self.exists('.tab-item.active'):
                # The existing TabBar compresses a long-name Tab at 400px. Select
                # context at normal size, then enter via the real + at target size.
                target = dict(self.rect['requested'])
                self.resize(1280, 800)
                self.click('.tab-item')
                self.resize(target['width'], target['height'])
            self.click('.tab-actions button:first-child')
        elif entry == "quick-actions":
            self.click('.tb-search-btn')
            self.click('.ts-view-pill', 'Actions')
            # The drill-down action, not the context-specific direct action.
            element = self.js("return [...document.querySelectorAll('.ts-action-row')].find(e=>e.innerText.includes('New chat in...'))")
            if not element:
                raise RuntimeError("Quick Actions New chat in... drill-down not found")
            self.request("POST", f"/element/{element[ELEMENT]}/click", {})
            self.click('.ts-row')
        self.wait(lambda: self.exists('.agent-picker'), "actual picker entry")
        self.wait(lambda: self.js("return document.fonts.status") == 'loaded', "fonts")
        time.sleep(.25)
        g = self.js(GEOMETRY)
        assert [b['label'] for b in g['buttons']] == LABELS + ['Open Settings'], "Fixture/ordering failure, not layout RED"
        assert PROJECT_NAME in g['texts'][0]['text'], "Wrong Project"
        self.report['interactions'].append({"entry": entry, "title": g['texts'][0]['text']})

    def dismiss(self):
        self.key(ESCAPE)
        self.wait(lambda: not self.exists('.agent-picker'), "Escape dismissal")

    def measure(self, name, entry):
        self.open(entry)
        g = self.js(GEOMETRY)
        defects = layout_defects(g)
        self.report['samples'].append({"name": name, "entry": entry, **self.rect,
                                       "geometry": g, "layoutDefects": defects})
        self.shot(name + '-' + entry)
        return defects

    def launch_log(self):
        file = self.home / 'fixture-launches.jsonl'
        return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []

    def terminal_count(self):
        return self.js("return document.querySelectorAll('.tab-item').length")

    def activation(self, label, activate, close=True):
        before = self.terminal_count()
        launches = len(self.launch_log())
        activate()
        self.wait(lambda: not self.exists('.agent-picker'), 'picker selection')
        self.wait(lambda: self.terminal_count() == before + 1, 'exactly one new Terminal')
        self.wait(lambda: len(self.launch_log()) == launches + 1, 'actual fixture process')
        time.sleep(.3)
        assert self.terminal_count() == before + 1
        added = self.launch_log()[launches:]
        assert len(added) == 1
        actual = added[0]
        assert actual['binary'] == {'Pi': 'pi', 'Shell': 'bash', 'Claude Code': 'claude'}[label]
        assert actual['cwd'] == self.f['project'] and actual['terminal']
        assert Path(f"/proc/{actual['pid']}/cwd").resolve() == Path(self.f['project'])
        self.report['interactions'].append({"activation": label, "process": actual, "terminalsBefore": before,
                                           "terminalsAfter": self.terminal_count()})
        self.shot(f"creation-{len(self.report['interactions'])}-{label.replace(' ', '-')}")
        if close:
            target = dict(self.rect['requested'])
            self.resize(1280, 800)
            self.click('.tab-item.active .tab-item-close')
            self.wait(lambda: self.terminal_count() == before, 'close owned Terminal')
            self.resize(target['width'], target['height'])

    def reveal(self, label):
        trace = []
        for _ in range(20):
            g = self.js(GEOMETRY)
            b = next(b for b in g['buttons'] if b['label'] == label)
            trace.append({"scrollTop": g['scrollTop'], "button": b})
            if visible_button(g, b):
                self.report['interactions'].append({"wheelReach": label, "trace": trace})
                return
            self.wheel(65 if b['rect']['bottom'] > g['dialog']['rect']['bottom'] - 1 else -65)
        raise AssertionError(f"Ordinary wheel cannot fully expose {label}: {trace}")

    def focus_label(self):
        g = self.js(GEOMETRY)
        return g['focus'], g['scrollTop']

    def traverse(self, activate=None, key=ENTER):
        # Existing chooser has no autofocus/trap. A native header click releases
        # the Terminal textarea's keyboard capture without forcing DOM focus.
        self.click('.agent-picker .picker-band')
        trace = []
        for _ in range(60):
            self.key(TAB)
            focus, scroll = self.focus_label()
            trace.append({"focus": focus, "scrollTop": scroll})
            if focus['option'] and focus['label'] == LABELS[0]:
                break
        else:
            self.report['interactions'].append({'failedKeyboardTraversal': trace})
            raise AssertionError('Tab never reaches first choice')
        for label in LABELS + ['Open Settings']:
            focus, scroll = self.focus_label()
            assert focus['label'] == label, trace
            g = self.js(GEOMETRY)
            b = next(b for b in g['buttons'] if b['label'] == label)
            if self.a.expect_layout == 'pass':
                assert visible_button(g, b), f'Focused button not exposed: {label}'
            if label == activate:
                self.report['interactions'].append({"keyboardTraversal": trace, "activate": label, "key": key})
                if label == 'Open Settings':
                    before, launches = self.terminal_count(), len(self.launch_log())
                    self.key(key)
                    self.wait(lambda: not self.exists('.agent-picker'), 'keyboard Settings')
                    assert self.js("return document.querySelector('.settings-view').offsetParent!==null")
                    assert self.terminal_count() == before and len(self.launch_log()) == launches
                else:
                    self.activation(label, lambda: self.key(key))
                return
            if label != 'Open Settings':
                self.key(TAB)
                trace.append({"focus": self.focus_label()[0], "scrollTop": self.focus_label()[1]})
        self.key(TAB, shift=True)
        assert self.focus_label()[0]['label'] == 'Shell'
        self.key(TAB, shift=True)
        assert self.focus_label()[0]['label'] == 'Pi'
        self.report['interactions'].append({"keyboardTraversal": trace, "shiftTab": ['Shell', 'Pi']})

    def detailed_minimum(self):
        self.resize(400, 300)
        self.open('home')
        for label in LABELS + ['Open Settings']:
            self.reveal(label)
        self.shot('minimum-bottom-dark')
        self.wheel(-2000)
        g = self.js(GEOMETRY)
        assert g['scrollTop'] == 0 and g['sections'][0]['rect']['top'] >= g['dialog']['rect']['top']
        assert g['dialog']['scrollHeight'] > g['dialog']['clientHeight']
        self.shot('minimum-top-dark')
        self.dismiss()
        for label in ['Pi', 'Shell']:
            self.open('tabbar')
            self.reveal(label)
            self.activation(label, lambda: self.click('.agent-picker-option', label))
        self.open('home')
        self.traverse()
        self.dismiss()
        for label, key in [('Pi', ENTER), ('Shell', SPACE), ('Open Settings', ENTER)]:
            self.open('tabbar')
            self.traverse(label, key)
        self.open('home')
        before = self.terminal_count()
        self.dismiss()
        assert self.terminal_count() == before
        self.open('home')
        self.request('POST', '/actions', {'actions': [{'type': 'pointer', 'id': 'mouse',
            'parameters': {'pointerType': 'mouse'}, 'actions': [
                {'type': 'pointerMove', 'x': 2, 'y': 2, 'duration': 0},
                {'type': 'pointerDown', 'button': 0}, {'type': 'pointerUp', 'button': 0}]}]})
        self.wait(lambda: not self.exists('.agent-picker'), 'outside native click cancellation')
        assert self.terminal_count() == before
        # Select Light through the existing native select, never mutate DOM/style/state.
        self.resize(1280, 800)
        self.click('.tb-settings-btn')
        self.click('.settings-nav-item', 'Appearance')
        self.click('.settings-body select')
        self.key('\ue010')  # End -> Light
        self.key(ENTER)
        assert self.js("return document.documentElement.dataset.theme") == 'light'
        self.resize(400, 300)
        self.open('home')
        assert not layout_defects(self.js(GEOMETRY))
        self.shot('minimum-top-light')
        self.reveal('Open Settings')
        self.shot('minimum-bottom-light')
        self.dismiss()
        self.resize(1280, 800)
        self.click('.tb-settings-btn')
        self.click('.settings-nav-item', 'Appearance')
        self.click('.settings-body select')
        self.key('\ue011')  # Home -> Dark
        self.key(ENTER)
        assert self.js("return document.documentElement.dataset.theme") == 'dark'
        self.open('quick-actions')
        assert not layout_defects(self.js(GEOMETRY))
        self.shot('quick-actions-same-project')
        self.dismiss()
        self.click('.ds-item:has(.ds-add)')
        assert self.exists('.picker:not(.agent-picker)')
        self.shot('add-projects-smoke')
        self.key(ESCAPE)

    def run(self):
        try:
            self.start()
            self.report['status'] = 'interaction-failure'
            defects = []
            for width, height, name, entries in [
                (1280, 800, 'AgentPickerDialog.full_registry_fits_normal_viewport', ['home']),
                (400, 600, 'AgentPickerDialog.full_registry_fits_narrow_viewport', ['home']),
                (400, 300, 'AgentPickerDialog.minimum_viewport_pointer_and_keyboard_activation', ['home'])]:
                self.resize(width, height)
                for entry in entries:
                    defects += self.measure(name, entry)
                    self.dismiss()
            if self.a.expect_layout == 'pass' and defects:
                self.report['layoutDefects'] = defects
                self.report['status'] = 'unexpected-layout-failure'
                raise AssertionError(defects)
            # A first-row fixture creates active Project context even on the broken release.
            self.resize(1280, 800)
            self.open('home')
            label = 'Pi' if self.a.expect_layout == 'pass' else 'Claude Code'
            self.activation(label, lambda: self.click('.agent-picker-option', label), close=False)
            for width, height, name in [
                (1280, 800, 'AgentPickerDialog.full_registry_fits_normal_viewport'),
                (800, 300, 'AgentPickerDialog.short_viewport_reaches_every_control'),
                (400, 300, 'AgentPickerDialog.home_and_tabbar_entry_points_keep_project')]:
                self.resize(width, height)
                defects += self.measure(name, 'tabbar')
                if width == 400 and self.a.expect_layout == 'fail':
                    self.traverse()  # Record pre-existing Tab/Shift+Tab order, no layout claim.
                self.dismiss()
            self.report['layoutDefects'] = defects
            if self.a.expect_layout == 'fail':
                if not any(d.startswith('horizontal:') for d in defects):
                    self.report['status'] = 'expected-layout-red-absent'
                    raise AssertionError('Expected horizontal defect absent')
            else:
                if defects:
                    self.report['status'] = 'unexpected-layout-failure'
                    raise AssertionError(defects)
                self.detailed_minimum()
            self.resize(1280, 800)
            while self.terminal_count():
                self.click('.tab-item .tab-item-close')
            if self.a.expect_layout == 'pass':
                self.stop()
                self.start()
                self.open('home')
                assert not layout_defects(self.js(GEOMETRY))
                self.shot('retained-restart-ask-full-registry')
                self.dismiss()
            self.report['status'] = 'candidate-green' if self.a.expect_layout == 'pass' else 'expected-layout-red'
        except Exception:
            self.report['error'] = traceback.format_exc()
            if self.sid and self.started:
                try:
                    self.shot('failure')
                except Exception:
                    pass
            raise
        finally:
            try:
                self.stop()
            finally:
                (self.output / 'report.json').write_text(json.dumps(self.report, indent=2))
                self.operations.close()


def visible_button(g, b):
    r, d, v = b['rect'], g['dialog']['rect'], g['viewport']
    return (r['width'] > 0 and r['height'] > 0 and b['hit'] and
            r['left'] >= d['left'] + 1 - 1 and r['right'] <= d['right'] - 1 + 1 and
            r['top'] >= max(0, d['top'] + 1) - 1 and
            r['bottom'] <= min(v['height'], d['bottom'] - 1) + 1)


def layout_defects(g):
    defects = []
    d, v, o = g['dialog']['rect'], g['viewport'], g['options']['rect']
    if d['left'] < -1 or d['right'] > v['width'] + 1:
        defects.append('horizontal: dialog exceeds viewport')
    if d['top'] < -1 or d['bottom'] > v['height'] + 1 or d['height'] > min(620, .86 * v['height']) + 1:
        defects.append('vertical: dialog exceeds viewport/max-height')
    if d['width'] > .92 * v['width'] + 1:
        defects.append('horizontal: dialog exceeds max-width')
    for name in ['dialog', 'options']:
        e = g[name]
        if e['scrollWidth'] > e['clientWidth'] + 1:
            defects.append(f'horizontal: {name} scrollWidth {e["scrollWidth"]} > clientWidth {e["clientWidth"]}')
    for i, b in enumerate(g['buttons']):
        r = b['rect']
        if r['width'] <= 0 or r['height'] <= 0:
            defects.append(f'hit area: {b["label"]}')
        left, right = (o['left'], o['right']) if i < 7 else (d['left'] + 1, d['right'] - 1)
        if r['left'] < left - 1 or r['right'] > right + 1:
            defects.append(f'horizontal: {b["label"]} button exceeds container')
        if b['scrollWidth'] > b['clientWidth'] + 1:
            defects.append(f'horizontal: {b["label"]} text overflows button')
        if r['top'] < d['top'] or r['bottom'] > d['bottom']:
            if g['dialog']['overflowY'] not in ['auto', 'scroll'] or g['dialog']['scrollHeight'] <= g['dialog']['clientHeight']:
                defects.append(f'vertical: {b["label"]} offscreen without root scroller')
        for t in b['textRanges']:
            if t['left'] < r['left'] - 1 or t['right'] > r['right'] + 1 or t['top'] < r['top'] - 1 or t['bottom'] > r['bottom'] + 1:
                defects.append(f'text: {b["label"]} range exceeds button')
        for other in g['buttons'][:i]:
            q = other['rect']
            if min(r['right'], q['right']) - max(r['left'], q['left']) > 1 and min(r['bottom'], q['bottom']) - max(r['top'], q['top']) > 1:
                defects.append(f'hit area overlap: {b["label"]}/{other["label"]}')
    for text in g['texts']:
        for r in text['ranges']:
            bounds = text['rect']
            if r['left'] < max(d['left'] + 1, bounds['left']) - 1 or r['right'] > min(d['right'] - 1, bounds['right']) + 1:
                defects.append(f'horizontal: text exceeds content area: {text["text"]}')
    return defects


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--home', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path, help='New evidence directory (never overwritten)')
    parser.add_argument('--port', required=True, type=int)
    parser.add_argument('--driver', type=Path, default=Path(__file__).resolve().parents[3] / 'bin/desktop-e2e')
    parser.add_argument('--expect-layout', choices=['pass', 'fail'], required=True)
    args = parser.parse_args()
    if len(str(args.home.resolve())) > 40:
        parser.error('Isolated HOME must be <=40 characters (Unix socket limit)')
    Runner(args).run()


if __name__ == '__main__':
    main()
