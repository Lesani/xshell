# Permission Prompt and composer fixtures

Raw PTY output of agent Terminals that show a Permission Prompt, the agent's chat composer or
another screen, for `xshell_core::prompt` (`tests/prompt_fixtures.rs`) and the Daemon's
`tests/permission_prompt.rs` and `tests/submit.rs`. Each `<name>.raw`
is the byte stream as the Terminal's reader would see it. Its `<name>.json` sidecar says how
to read it:

```json
{
  "agent": "claude",            // or "codex": selects the extractor's markers and evidence
  "version": "2.1.296",         // the agent version the screen follows
  "cols": 100, "rows": 30,      // the screen size to feed it at
  "synthetic": true,            // true: generated; false: recorded from a real session
  "source": "…",                // how it was made
  "expect": {                   // null: the screen shows no prompt that may get buttons
    "textContains": ["…"],      // substrings of the prompt's text
    "options": ["…"]            // the labels, in order (key hints removed)
  },
  "composer": false,            // whether the screen ends with the agent's chat composer
  "images": 0                   // composers only: the `[Image #n]` chips in its input
}
```

## The current fixtures are synthetic

No real agent session was started to make them (xshell-remote#8, lead decision D9: no
spending on an account, no network model provider). `gen_synthetic.py` builds them from what
the installed CLIs contain, read from their binaries without running a session:

- **Claude Code 2.1.296**: the dialog question `Do you want to proceed?` (and the per-tool
  ones such as `Do you want to make this edit to <file>?` and
  `Do you want to allow Claude to fetch this content?`), the option labels (`Yes`,
  `Yes, and don't ask again for …`, `No, and tell Claude what to do differently (esc)`), the
  Select component's `❯` focus marker and `n.` indexes, labels wrapping under themselves, and
  Ink's way of drawing: whole lines, the dynamic region erased (`ESC[2K ESC[1A`) and redrawn.
  Two dialog shapes: a top rule without side frame, and a round box with sides.
- **codex-cli 0.154.0**: the approval questions (`Would you like to run the following
  command?`, `… make the following edits?`), the option labels with their key hints
  (`Yes, proceed (y)`, `… (a)`, `No, and tell Codex what to do differently (esc)`), the `›`
  focus marker, and the key-hint footer `Press enter to confirm or esc to cancel`, drawn the
  way ratatui's inline viewport draws: cells placed with CUP at the bottom of the screen.

- **The chat composers** (`term.submit`): Claude Code 2.x draws its input box without sides
  (`borderStyle: "round"`, `borderLeft`/`borderRight` off: a rule of `─` above and below),
  with the prompt mark `❯` (`figures.pointer`) and the footer (`? for shortcuts`). Codex draws
  its bottom pane with the `›` mark, the placeholder `Ask Codex to do anything`, blank padding
  rows and the footer (`? for shortcuts`, `100% context left`). Screens that are not the
  composer: each agent's model picker (`Select model` / `Select Model`, a numbered list with
  its focus), Codex's list with an unnumbered `›` focus and its own footer, Claude Code's
  bash mode (`!`), a command's `[y/N]` question below the input, and a custom status line
  (the composer's footer is recognised only from the agents' own hints). An attached image
  (`term.submit` files): both agents put the chip `[Image #n]` in the input
  (`claude-composer-image`, `codex-composer-image`); while Claude Code still reads a pasted
  image its footer says `Pasting…` (`claude-composer-pasting`, not the composer).

`claude-idle` (no prompt; the older round input box with sides), `unknown-elicitation` (an MCP
form; text-only) and the composer and picker screens have `expect: null`; `composer` is true
for `claude-idle` and the `*-composer-*` screens except `claude-composer-pasting`. Regenerate with `python3 -I gen_synthetic.py .` from this directory.

What synthetic fixtures cannot show is whether the real TUIs match. Until real recordings
replace them, a composer the Daemon does not recognise refuses replies (`agent does not
accept pasted input yet`) rather than typing into something else. Until real recordings
replace them, extraction is strict (an unrecognised screen gives a text-only prompt, never
buttons), and an answer the TUI ignores is published again under a new id (the Daemon's
recheck).

## Recording real fixtures (xshell-remote#17, HITL)

On a machine with a Claude Code and a Codex login, in a throwaway directory:

1. Note the versions: `claude --version`, `codex --version`.
2. For each size (`100x30`, then `50x30` for the wrapped labels), in a terminal of exactly that
   size (`stty cols 100 rows 30`):

   ```sh
   script -q -E never -O claude-bash-100x30.raw -c 'claude'
   ```

   Ask for something that needs one permission (`list the files in /tmp` gives a Bash
   prompt), wait until the dialog is fully drawn, then press Esc and exit (`/exit`). Codex the
   same way (`script … -O codex-exec-100x30.raw -c codex`, and `make a one-line edit to
   README.md` for `codex-edits`). Claude also: an Edit prompt (`claude-edit-100x30`) and a
   WebFetch prompt with a "don't ask again for <domain>" option
   (`claude-webfetch-domain-100x30`).
3. Cut each `.raw` after the dialog was drawn and before the answer (the bytes after it are
   the answer's redraw).
4. Write the sidecar with `"synthetic": false`, the version, the size and the `expect` the
   screen shows, and replace the synthetic file of the same name.
5. The composers the same way: each agent idle (`claude-composer-idle`,
   `codex-composer-idle`), while working, with a two-line draft, and after `/model`
   (`claude-model-picker`, `codex-model-picker`); cut each after the screen was drawn. Their
   sidecars say `"composer"`. Then reply once through the bracketed paste and Enter the
   Daemon types (`printf '\033[200~hello\033[201~'`, a pause, then Enter), idle and while
   working, and note that the message is submitted once, as typed.
6. Remove the sessions this left (`~/.claude/projects/<the throwaway directory>`, the Codex
   rollout) and check that `~/.claude/settings.json` is unchanged.

### Key gate

In the same setup, answer one prompt per option by typing only that option's digit (no
Enter) and note what each agent does: whether the digit alone submits the option, and which
option it picks. Record it here:

| Agent | Version | `1` | `2` | `3` | Digit submits? |
|---|---|---|---|---|---|
| Claude Code | (to record) | | | | |
| Codex | (to record) | | | | |

If a digit only moves the focus, the extractor must send arrows from the focused option and
then Enter instead (`FoundOption::keys`); nothing else changes.
