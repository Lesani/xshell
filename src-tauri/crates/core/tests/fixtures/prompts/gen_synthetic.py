#!/usr/bin/env python3
"""Generate the synthetic Permission Prompt fixtures (xshell-remote#8, lead decision D9).

No agent session is started. The dialogs are built from the strings in the installed CLIs
(Claude Code 2.1.296, codex-cli 0.154.0) and drawn the way each TUI draws: Claude Code with
Ink (whole lines, the dynamic region erased with ESC[2K ESC[1A and redrawn), Codex with
ratatui (an inline viewport at the bottom, cells placed with CUP).

Usage: gen_synthetic.py <out-dir>
"""
import json
import os
import sys
import textwrap

CLAUDE_VERSION = "2.1.296"
CODEX_VERSION = "0.154.0"
ROWS = 30

ESC = "\x1b"
HIDE = ESC + "[?25l"
SHOW = ESC + "[?25h"
BOLD, DIM, RESET = ESC + "[1m", ESC + "[2m", ESC + "[0m"
FOCUS = ESC + "[38;5;153m"
WARN = ESC + "[38;5;220m"


def wrap(text, width, first_indent, rest_indent):
    lines = textwrap.wrap(text, width=width - len(first_indent), break_long_words=True,
                          break_on_hyphens=False)
    if not lines:
        return [first_indent]
    out = [first_indent + lines[0]]
    rest = " ".join(lines[1:])
    if rest:
        for l in textwrap.wrap(rest, width=width - len(rest_indent), break_long_words=True,
                               break_on_hyphens=False):
            out.append(rest_indent + l)
    return out


# ---- Claude Code (Ink) ----------------------------------------------------------------------

def ink_erase(n):
    """Ink's eraseLines(n): clear each line of the previous frame, bottom up."""
    return (ESC + "[2K" + ESC + "[1A") * (n - 1) + ESC + "[2K" + ESC + "[G"


def claude_banner(cols):
    w = min(cols, 60)
    inner = w - 2
    title = " ✻ Welcome to Claude Code!"
    return [
        "╭" + "─" * inner + "╮",
        "│" + title.ljust(inner) + "│",
        "│" + "".ljust(inner) + "│",
        "│" + "   cwd: /home/dev/proj".ljust(inner)[:inner] + "│",
        "╰" + "─" * inner + "╯",
        "",
    ]


def claude_input_box(cols):
    inner = cols - 2
    return [
        "╭" + "─" * inner + "╮",
        "│ > " + " " * (inner - 3) + "│",
        "╰" + "─" * inner + "╯",
        DIM + "  ? for shortcuts" + RESET,
    ]


def claude_composer(cols, lines=("",), mark="❯", footer="? for shortcuts"):
    """Claude Code 2.x prompt input: its round box without sides (a rule above and below),
    the prompt mark, continuation lines indented under the input, and the footer."""
    rule = DIM + "─" * cols + RESET
    out = [rule, mark + " " + lines[0]]
    out.extend("  " + l for l in lines[1:])
    out.append(rule)
    out.append(DIM + "  " + footer + RESET)
    return out


def claude_options(cols, labels):
    """The Select: `❯ 1. label` focused, `  n. label` otherwise; labels wrap under themselves."""
    out = []
    for i, label in enumerate(labels, 1):
        lead = " ❯ " if i == 1 else "   "
        head = f"{lead}{i}. "
        rows = wrap(label, cols - 1, head, " " * len(head))
        if i == 1:
            rows = [FOCUS + r + RESET for r in rows]
        out.extend(rows)
    return out


def claude_rule_dialog(cols, title, body, question, labels, footer):
    """Claude Code 2.x permission dialog: a top rule, padded content, no side frame."""
    out = [DIM + "─" * cols + RESET, " " + BOLD + title + RESET, ""]
    for b in body:
        out.extend(wrap(b, cols - 1, "   ", "   "))
    out.append("")
    out.extend(wrap(question, cols - 1, " ", " "))
    out.extend(claude_options(cols, labels))
    out.append("")
    out.append(" " + DIM + footer + RESET)
    return out


def claude_box_dialog(cols, title, body, question, labels):
    """The framed variant (round box with sides), as file edits draw it."""
    inner = cols - 2

    def boxed(s):
        # Visible width without SGR.
        vis = s
        for code in (BOLD, DIM, RESET, FOCUS, WARN):
            vis = vis.replace(code, "")
        return "│" + s + " " * max(0, inner - len(vis)) + "│"

    out = ["╭" + "─" * inner + "╮", boxed(" " + BOLD + title + RESET)]
    for b in body:
        for r in wrap(b, inner - 1, " ", " "):
            out.append(boxed(r))
    out.append(boxed(""))
    for r in wrap(question, inner - 1, " ", " "):
        out.append(boxed(r))
    for r in claude_options(inner, labels):
        out.append(boxed(r))
    out.append("╰" + "─" * inner + "╯")
    return out


def claude_stream(cols, prompt_text, reply, dialog):
    """The whole session as Ink writes it: banner and input, a thinking frame, then the
    reply (static) and the dialog in place of the input."""
    s = HIDE
    static = claude_banner(cols)
    s += "\r\n".join(static) + "\r\n"
    frame = claude_input_box(cols)
    s += "\r\n".join(frame)
    # The user submits: the prompt goes static, a spinner frame replaces the input.
    s += ink_erase(len(frame))
    s += "> " + prompt_text + "\r\n\r\n"
    frame = [WARN + "✻ Thinking… " + RESET + DIM + "(esc to interrupt)" + RESET, ""] + claude_input_box(cols)
    s += "\r\n".join(frame)
    s += ink_erase(len(frame))
    s += "⏺ " + reply + "\r\n\r\n"
    s += "\r\n".join(dialog)
    return s


def claude_fixtures(cols):
    out = {}
    bash = claude_rule_dialog(
        cols, "Bash command",
        ["ls -la /tmp", DIM + "List the files in /tmp" + RESET],
        "Do you want to proceed?",
        ["Yes",
         "Yes, and don't ask again for ls commands in /home/dev/proj",
         "No, and tell Claude what to do differently " + BOLD + "(esc)" + RESET],
        "Esc to cancel",
    )
    out["claude-bash"] = (
        claude_stream(cols, "list the files in /tmp", "I'll list the files in /tmp.", bash),
        {"textContains": ["Bash command", "ls -la /tmp", "Do you want to proceed?"],
         "options": ["Yes",
                     "Yes, and don't ask again for ls commands in /home/dev/proj",
                     "No, and tell Claude what to do differently"]},
    )
    edit = claude_box_dialog(
        cols, "Edit file",
        ["src/main.rs", "  12 -    println!(\"hello\");", "  12 +    println!(\"hello, world\");"],
        "Do you want to make this edit to main.rs?",
        ["Yes",
         "Yes, allow all edits during this session " + BOLD + "(shift+tab)" + RESET,
         "No, and tell Claude what to do differently " + BOLD + "(esc)" + RESET],
    )
    out["claude-edit"] = (
        claude_stream(cols, "greet the world", "I'll update the greeting.", edit),
        {"textContains": ["Edit file", "src/main.rs", "Do you want to make this edit to main.rs?"],
         "options": ["Yes",
                     "Yes, allow all edits during this session",
                     "No, and tell Claude what to do differently"]},
    )
    fetch = claude_rule_dialog(
        cols, "Fetch",
        ["https://example.com/docs", DIM + "Claude wants to fetch content from example.com" + RESET],
        "Do you want to allow Claude to fetch this content?",
        ["Yes",
         "Yes, and don't ask again for example.com",
         "No, and tell Claude what to do differently " + BOLD + "(esc)" + RESET],
        "Esc to cancel",
    )
    out["claude-webfetch-domain"] = (
        claude_stream(cols, "read the docs", "I'll fetch the docs page.", fetch),
        {"textContains": ["https://example.com/docs",
                          "Do you want to allow Claude to fetch this content?"],
         "options": ["Yes",
                     "Yes, and don't ask again for example.com",
                     "No, and tell Claude what to do differently"]},
    )
    idle = HIDE + "\r\n".join(claude_banner(cols)) + "\r\n"
    idle += "> fix the bug\r\n\r\n⏺ Fixed the off-by-one in parse().\r\n\r\n"
    idle += "\r\n".join(claude_input_box(cols))
    out["claude-idle"] = (idle, None)
    # An MCP elicitation form: a question that is not answered with one key.
    inner = cols - 2
    form = [
        "╭" + "─" * inner + "╮",
        "│" + " MCP server \"tracker\" requests input".ljust(inner) + "│",
        "│" + "".ljust(inner) + "│",
        "│" + " Which project should the issue go to?".ljust(inner) + "│",
        "│" + " Project: █".ljust(inner) + "│",
        "│" + " Labels:  ◉ bug  ◯ feature".ljust(inner) + "│",
        "│" + "".ljust(inner) + "│",
        "│" + " Enter to submit · Esc to decline".ljust(inner) + "│",
        "╰" + "─" * inner + "╯",
    ]
    elic = claude_stream(cols, "file an issue", "I'll file the issue.", form)
    out["unknown-elicitation"] = (elic, None)
    # The chat composer (term.submit): idle, while working, with a draft; and screens that
    # are not it: the model picker and the bash mode.
    hist = HIDE + "\r\n".join(claude_banner(cols)) + "\r\n"
    hist += "> fix the bug\r\n\r\n⏺ Fixed the off-by-one in parse().\r\n\r\n"
    out["claude-composer-idle"] = (hist + "\r\n".join(claude_composer(cols)), None)
    working = [WARN + "✻ Thinking… " + RESET + DIM + "(esc to interrupt)" + RESET, ""] + claude_composer(cols)
    out["claude-composer-working"] = (hist + "\r\n".join(working), None)
    draft = claude_composer(cols, ("first line of a draft", "second line"))
    out["claude-composer-draft"] = (hist + "\r\n".join(draft), None)
    # A pasted image path being read (`Pasting…` in place of the footer hints: not the
    # composer), then attached as a chip in the input.
    pasting = claude_composer(cols, footer="Pasting…")
    out["claude-composer-pasting"] = (hist + "\r\n".join(pasting), None)
    image = claude_composer(cols, ("[Image #1] ",))
    out["claude-composer-image"] = (hist + "\r\n".join(image), None)
    bash_mode = claude_composer(cols, ("ls",), mark="!", footer="! for shell mode")
    out["claude-bash-mode"] = (hist + "\r\n".join(bash_mode), None)
    picker = [DIM + "─" * cols + RESET, " " + BOLD + "Select model" + RESET]
    picker += wrap("Switch between Claude models. Your pick becomes the default for new "
                   "sessions. For other/previous model names, specify with --model.",
                   cols - 1, " ", " ")
    picker.append("")
    picker += claude_options(cols, ["Default (recommended)", "Opus", "Sonnet", "Haiku"])
    picker += ["", " " + DIM + "Enter to confirm · Esc to exit" + RESET]
    frame = claude_composer(cols)
    picked = hist + "\r\n".join(frame) + ink_erase(len(frame)) + "\r\n".join(picker)
    out["claude-model-picker"] = (picked, None)
    # Not the composer either: a command's question printed below it, and a custom status
    # line the footer does not know.
    asked = claude_composer(cols) + ["Continue? [y/N] "]
    out["claude-composer-question"] = (hist + "\r\n".join(asked), None)
    status = claude_composer(cols)[:-1] + [DIM + "  main • 3 files changed • $0.42" + RESET]
    out["claude-composer-statusline"] = (hist + "\r\n".join(status), None)
    return out


# ---- Codex (ratatui inline viewport) ----------------------------------------------------------

def cup(row, col=1):
    return f"{ESC}[{row};{col}H"


def codex_viewport(cols, lines):
    """Draw `lines` as the bottom rows of the screen, each placed with CUP and cleared to
    the end of the line, as ratatui's inline viewport flushes its buffer."""
    top = ROWS - len(lines) + 1
    s = ""
    for i, l in enumerate(lines):
        s += cup(top + i) + l + ESC + "[K"
    return s


def codex_options(cols, labels):
    out = []
    for i, label in enumerate(labels, 1):
        lead = "› " if i == 1 else "  "
        head = f"{lead}{i}. "
        rows = wrap(label, cols, head, " " * len(head))
        if i == 1:
            rows = [ESC + "[36m" + r + ESC + "[39m" for r in rows]
        out.extend(rows)
    return out


def codex_dialog(cols, question, body, labels):
    out = []
    out.extend(wrap(question, cols, "  ", "  "))
    out.append("")
    for b in body:
        out.extend(wrap(b, cols, "  ", "    "))
        out.append("")
    out.extend(codex_options(cols, labels))
    out.append("")
    out.append("  " + DIM + "Press " + RESET + "enter" + DIM + " to confirm or " + RESET + "esc"
               + DIM + " to cancel" + RESET)
    return out


def codex_stream(cols, prompt_text, dialog):
    s = HIDE + ESC + "[2J" + cup(1)
    hist = [
        BOLD + ">_ OpenAI Codex" + RESET + " (v" + CODEX_VERSION + ")",
        "",
        DIM + "model: " + RESET + "gpt-5-codex   " + DIM + "directory: " + RESET + "~/proj",
        "",
        BOLD + "user" + RESET,
        prompt_text,
        "",
    ]
    for i, l in enumerate(hist, 1):
        s += cup(i) + l
    # The composer while working, then the approval overlay in its place.
    working = ["", "• Working (3s • esc to interrupt)", "", "› " + DIM + "Ask Codex to do anything" + RESET, ""]
    s += codex_viewport(cols, working)
    blank = [""] * len(dialog)
    s += codex_viewport(cols, blank)
    s += codex_viewport(cols, dialog)
    return s


def codex_composer(lines=("" + DIM + "Ask Codex to do anything" + RESET,), status=None):
    """The bottom pane: an optional status row, the composer with its blank padding rows
    and the prompt mark, then the footer."""
    out = [""]
    if status:
        out += [status, ""]
    out.append("› " + lines[0])
    out.extend("  " + l for l in lines[1:])
    out += ["", "  " + DIM + "? for shortcuts" + RESET + "                    100% context left"]
    return out


def codex_history(prompt_text):
    s = HIDE + ESC + "[2J" + cup(1)
    hist = [
        BOLD + ">_ OpenAI Codex" + RESET + " (v" + CODEX_VERSION + ")",
        "",
        DIM + "model: " + RESET + "gpt-5-codex   " + DIM + "directory: " + RESET + "~/proj",
        "",
        "› " + prompt_text,
        "",
        "• Fixed the off-by-one in parse().",
    ]
    for i, l in enumerate(hist, 1):
        s += cup(i) + l
    return s


def codex_fixtures(cols):
    out = {}
    base = codex_history("fix the bug")
    out["codex-composer-idle"] = (base + codex_viewport(cols, codex_composer()), None)
    working = codex_composer(status="• Working (3s • esc to interrupt)")
    out["codex-composer-working"] = (base + codex_viewport(cols, working), None)
    draft = codex_composer(("first line of a draft", "second line"))
    out["codex-composer-draft"] = (base + codex_viewport(cols, draft), None)
    image = codex_composer(("[Image #1] ",))
    out["codex-composer-image"] = (base + codex_viewport(cols, image), None)
    picker = ["", "  " + BOLD + "Select Model" + RESET,
              "  " + DIM + "Pick a quick auto mode or browse all models." + RESET, ""]
    picker += codex_options(cols, ["auto", "gpt-5-codex", "gpt-5"])
    picker += ["", "  " + DIM + "Press enter to confirm or esc to go back" + RESET]
    blank = [""] * len(picker)
    picked = (base + codex_viewport(cols, codex_composer()) + codex_viewport(cols, blank)
              + codex_viewport(cols, picker))
    out["codex-model-picker"] = (picked, None)
    # An unnumbered list with the composer's mark as its focus, and its own footer.
    unnumbered = ["", "  " + BOLD + "Select Model" + RESET, "", "› auto", "  gpt-5-codex",
                  "  gpt-5", "", "  " + DIM + "Press enter to confirm or esc to go back" + RESET]
    blank = [""] * len(unnumbered)
    listed = (base + codex_viewport(cols, codex_composer()) + codex_viewport(cols, blank)
              + codex_viewport(cols, unnumbered))
    out["codex-unnumbered-picker"] = (listed, None)
    exec_d = codex_dialog(
        cols, "Would you like to run the following command?",
        ["Reason: list the files in /tmp", "$ ls -la /tmp"],
        ["Yes, proceed (y)",
         "Yes, and don't ask again for this command in this session (a)",
         "No, and tell Codex what to do differently (esc)"],
    )
    out["codex-exec"] = (
        codex_stream(cols, "list the files in /tmp", exec_d),
        {"textContains": ["Would you like to run the following command?", "$ ls -la /tmp"],
         "options": ["Yes, proceed",
                     "Yes, and don't ask again for this command in this session",
                     "No, and tell Codex what to do differently"]},
    )
    edits = codex_dialog(
        cols, "Would you like to make the following edits?",
        ["src/main.rs (+1 -1)"],
        ["Yes, proceed (y)",
         "Yes, and don't ask again for these files (a)",
         "No, and tell Codex what to do differently (esc)"],
    )
    out["codex-edits"] = (
        codex_stream(cols, "greet the world", edits),
        {"textContains": ["Would you like to make the following edits?", "src/main.rs"],
         "options": ["Yes, proceed",
                     "Yes, and don't ask again for these files",
                     "No, and tell Codex what to do differently"]},
    )
    return out


# The screens that end with the agent's chat composer; every other one does not.
COMPOSERS = {"claude-idle", "claude-composer-idle", "claude-composer-working",
             "claude-composer-draft", "codex-composer-idle", "codex-composer-working",
             "codex-composer-draft", "claude-composer-image", "codex-composer-image"}
# The composers with an attached image chip, and how many.
IMAGES = {"claude-composer-image": 1, "codex-composer-image": 1}


def main():
    dest = sys.argv[1]
    os.makedirs(dest, exist_ok=True)
    plan = [
        ("claude", CLAUDE_VERSION, 100, ["claude-bash", "claude-edit", "claude-webfetch-domain",
                                         "claude-idle", "unknown-elicitation",
                                         "claude-composer-idle", "claude-composer-working",
                                         "claude-composer-draft", "claude-bash-mode",
                                         "claude-model-picker", "claude-composer-question",
                                         "claude-composer-statusline",
                                         "claude-composer-pasting", "claude-composer-image"]),
        ("claude", CLAUDE_VERSION, 50, ["claude-bash"]),
        ("codex", CODEX_VERSION, 100, ["codex-exec", "codex-edits", "codex-composer-idle",
                                       "codex-composer-working", "codex-composer-draft",
                                       "codex-model-picker", "codex-unnumbered-picker",
                                       "codex-composer-image"]),
        ("codex", CODEX_VERSION, 50, ["codex-exec"]),
    ]
    for agent, version, cols, names in plan:
        made = claude_fixtures(cols) if agent == "claude" else codex_fixtures(cols)
        for name in names:
            raw, expect = made[name]
            base = name if expect is None else f"{name}-{cols}x{ROWS}"
            with open(os.path.join(dest, base + ".raw"), "wb") as f:
                f.write(raw.encode("utf-8"))
            side = {
                "agent": agent,
                "version": version,
                "cols": cols,
                "rows": ROWS,
                "synthetic": True,
                "source": "gen_synthetic.py: built from the CLI's strings and rendering, "
                          "not recorded (xshell-remote#8, D9)",
                "expect": expect,
                # Whether the screen ends with the agent's chat composer (term.submit).
                "composer": name in COMPOSERS,
            }
            if name in COMPOSERS:
                # The image chips in its composer (term.submit files).
                side["images"] = IMAGES.get(name, 0)
            with open(os.path.join(dest, base + ".json"), "w") as f:
                json.dump(side, f, indent=2, ensure_ascii=False)
                f.write("\n")


if __name__ == "__main__":
    main()
