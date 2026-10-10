// A Claude-like fake agent on Node (tests/windows_submit.rs, node_reader_is_safe): stdin in
// raw mode as Node's runtime sets it, bracketed paste on, the composer fixture drawn. It
// logs to its working directory as the console fake does: modes.json, input.bin,
// reads.log, submits.jsonl, sizes.log, then started.
const fs = require('fs');
const path = require('path');
const dir = process.cwd();
const at = (f) => path.join(dir, f);
const START = Buffer.from('\x1b[200~');
const END = Buffer.from('\x1b[201~');

process.stdin.setRawMode(true);
fs.writeFileSync(
  at('modes.json'),
  JSON.stringify({ reader: 'node', node: process.version, uv: process.versions.uv, isTTY: !!process.stdin.isTTY })
);
process.stdout.write('\x1b[H\x1b[2J');
process.stdout.write(fs.readFileSync(process.env.FAKE_FIXTURE));
process.stdout.write('\x1b[?2004h');
process.stdout.on('resize', () => {
  fs.appendFileSync(at('sizes.log'), `${process.stdout.columns} ${process.stdout.rows}\n`);
});

let buf = Buffer.alloc(0);
let pos = 0;
let inPaste = false;
let draft = [];
const isPrefix = (whole, part) => part.length < whole.length && whole.subarray(0, part.length).equals(part);
process.stdin.on('data', (d) => {
  fs.appendFileSync(at('input.bin'), d);
  fs.appendFileSync(at('reads.log'), `${d.length}\n`);
  buf = Buffer.concat([buf, d]);
  for (;;) {
    const rest = buf.subarray(pos);
    if (rest.length === 0) break;
    if (rest.subarray(0, 6).equals(START)) { inPaste = true; pos += 6; continue; }
    if (rest.subarray(0, 6).equals(END)) { inPaste = false; pos += 6; continue; }
    if (isPrefix(START, rest) || isPrefix(END, rest)) break;
    pos += 1;
    if (rest[0] === 0x0d && !inPaste) {
      fs.appendFileSync(at('submits.jsonl'), JSON.stringify(Buffer.from(draft).toString('utf8')) + '\n');
      draft = [];
    } else {
      draft.push(rest[0]);
    }
  }
});
fs.writeFileSync(at('started'), '');
