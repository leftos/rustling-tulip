// TLS-intercepting CONNECT proxy + Anthropic API mock for RT-2189 (Heads up notice captures).
// node intercept.mjs <certPem> <keyPem> <portFile> <logFile>
// The CLI gets HTTPS_PROXY=http://127.0.0.1:<port> and NODE_EXTRA_CA_CERTS=<throwaway CA>. A CONNECT to a host in
// ANSWERED is accepted and its TLS terminated here with a leaf the throwaway CA signed; every other CONNECT and every
// plain-HTTP proxy request is refused (403) and logged, so no request leaves the machine.
// Routes: /v1/messages (main turns and the You-should-know side requests, as ../mock.mjs), /v1/messages/count_tokens,
// /api/eval[-authed]/<key> (GrowthBook remote eval: tengu_jolly_dewdrop on), /api/oauth/profile, /api/oauth/claude_cli/roles,
// /api/event_logging/batch; anything else 404 and logged.
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs';
const [certFile, keyFile, portFile, logFile] = process.argv.slice(2);
const ANSWERED = new Set(['api.anthropic.com', 'platform.claude.com', 'claude.ai', 'console.anthropic.com', 'mcp-proxy.anthropic.com', 'downloads.claude.ai', 'statsig.anthropic.com', 'code.claude.com']);
const FLAGS = JSON.parse(process.env.RT2189_FLAGS || '{"tengu_jolly_dewdrop":true}');
let n = 0;
const log = (o) => fs.appendFileSync(logFile, JSON.stringify({ t: new Date().toISOString(), ...o }) + '\n');
const textOf = (msg) => !msg ? '' : typeof msg.content === 'string' ? msg.content : msg.content.filter((b) => b.type === 'text').map((b) => b.text).join('\n');
const hasToolResult = (msg) => !!msg && Array.isArray(msg.content) && msg.content.some((b) => b.type === 'tool_result');
function lastUser(ms) { for (let i = ms.length - 1; i >= 0; i--) if (ms[i].role === 'user') return ms[i]; return null; }
function lastTyped(ms) { for (let i = ms.length - 1; i >= 0; i--) if (ms[i].role === 'user' && !hasToolResult(ms[i]) && /\[\[S:/.test(textOf(ms[i]))) return i; return -1; }
const bullets = (k) => Array.from({ length: k }, (_, i) => '- Step ' + (i + 1) + ': the nightly job copies note ' + (i + 1) + ' to the shared drive.').join('\n');
const YSK = {
  long: { line: 'The nightly sync job you set up earlier copies every note in this folder, including the private ones, to a shared drive that anyone with the link can open, so the private notes are readable by people outside your team right now.',
    explanation: '**Why this note matters**\n' + bullets(12) + '\n- **Consider moving the job output somewhere private.**' },
  short: { line: 'Note six says a nightly job posts your files to a shared drive anyone can read.',
    explanation: '**Why this note matters**\n- The nightly job posts what it finds to a shared drive.\n- Anyone with the link can read it there.' },
  tall: { line: 'Note six says a nightly job posts your files to a shared drive anyone can read.',
    explanation: '**Why this note matters**\n' + bullets(12) + '\n- **Consider moving the job output somewhere private.**' },
  lazy: { line: 'A nightly job copies this folder, private notes included, to a shared drive.', explanation: '**Why this note matters**\n' + bullets(12) + '\n- **Consider moving the job output somewhere private.**', lazy: true },
  noexplain: { line: 'Note six says a nightly job posts your files to a shared drive anyone can read.', explanation: '' },
};
// [[YSK:<variant>]] or [[YSK:<variant>:<tag>]]; the last marker in the conversation wins. A tag makes the line unique
// (the plugin does not show a line it has shown before), as "(case <tag>)" at its end.
function variant(ms) { let v = { ...YSK.long }; for (const m of ms) { for (const k of textOf(m).matchAll(/\[\[YSK:(\w+)(?::(\w+))?\]\]/g)) if (YSK[k[1]]) { v = { ...YSK[k[1]] }; if (k[2]) v.line = v.line.replace(/\.$/, ' (case ' + k[2] + ').'); } } return v; }
function plan(body) {
  const ms = body.messages || [];
  const u = lastUser(ms); const ut = textOf(u);
  if (ut.includes('"You should know" feature')) {
    const v = variant(ms);
    if (ut.includes('The person watching you work said yes to')) return { kind: 'ysk-explain', text: v.explanation || 'Nothing more to add.', delay: v.lazy ? Number(process.env.RT2189_EXPLAIN_DELAY || 2000) : 0 };
    return { kind: 'ysk-offer', text: 'learn: ' + v.line + '\ntag: Heads up' + (v.explanation && !v.lazy ? '\nexplain:\n' + v.explanation : '') };
  }
  const ti = lastTyped(ms);
  const typed = ti >= 0 ? textOf(ms[ti]) : '';
  const st = typed.match(/\[\[S:steps:(\d+)(?::(\d+))?\]\]/);
  if (st) {
    const done = ms.slice(ti + 1).filter(hasToolResult).length;
    if (done < Number(st[1])) return { kind: 'step', step: done, delay: Number(st[2] || 0), tool: 'Glob', input: { pattern: '*.md' } };
    return { kind: 'step-end', text: 'Done after ' + done + ' steps.' };
  }
  return { kind: 'plain', text: 'ok' };
}
function sse(res, model, p) {
  const id = 'msg_mock_' + ++n;
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', 'request-id': 'req_mock_' + n });
  const ev = (type, data) => res.write('event: ' + type + '\ndata: ' + JSON.stringify({ type, ...data }) + '\n\n');
  ev('message_start', { message: { id, type: 'message', role: 'assistant', model, content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 1, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 } } });
  if (p.tool) {
    ev('content_block_start', { index: 0, content_block: { type: 'tool_use', id: 'toolu_mock_' + n, name: p.tool, input: {} } });
    ev('content_block_delta', { index: 0, delta: { type: 'input_json_delta', partial_json: JSON.stringify(p.input) } });
  } else {
    ev('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
    ev('content_block_delta', { index: 0, delta: { type: 'text_delta', text: p.text } });
  }
  ev('content_block_stop', { index: 0 });
  ev('message_delta', { delta: { stop_reason: p.tool ? 'tool_use' : 'end_turn', stop_sequence: null }, usage: { output_tokens: 5 } });
  ev('message_stop', {});
  res.end();
}
function json(res, model, p) {
  const content = p.tool ? [{ type: 'tool_use', id: 'toolu_mock_' + ++n, name: p.tool, input: p.input }] : [{ type: 'text', text: p.text }];
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(JSON.stringify({ id: 'msg_mock_' + n, type: 'message', role: 'assistant', model, content, stop_reason: p.tool ? 'tool_use' : 'end_turn', stop_sequence: null, usage: { input_tokens: 10, output_tokens: 5 } }));
}
const send = (res, code, obj) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(obj)); };
const ACCOUNT = { uuid: '00000000-0000-4000-8000-000000002189', email: 'rt2189@example.invalid', display_name: 'Scratch', full_name: 'Scratch' };
const ORG = { uuid: '00000000-0000-4000-8000-00000000a189', name: 'Scratch org', organization_type: 'claude_max', billing_type: 'stripe_subscription', rate_limit_tier: 'default_claude_max_20x', has_extra_usage_enabled: false };
function route(req, res, raw) {
  const host = req.headers.host || req.socket.servername || '?';
  const url = req.url || '';
  let body = null;
  try { body = raw ? JSON.parse(raw) : null; } catch { body = { unparsed: raw.slice(0, 2000) }; }
  const base = { host, method: req.method, url, headerNames: Object.keys(req.headers).filter((h) => !/^(authorization|x-api-key|cookie)$/i.test(h)) };
  if (url.startsWith('/__quit')) { log({ ...base }); res.end('bye'); setTimeout(() => process.exit(0), 100); return; }
  if (host.startsWith('api.anthropic.com') && req.method === 'POST' && url.startsWith('/v1/messages/count_tokens')) { log(base); return send(res, 200, { input_tokens: 100 }); }
  if (host.startsWith('api.anthropic.com') && req.method === 'POST' && url.startsWith('/v1/messages')) {
    const p = plan(body || {});
    const lu = textOf(lastUser((body || {}).messages || []));
    log({ ...base, reply: p, stream: !!(body && body.stream), nmsg: ((body || {}).messages || []).length, lastUser: lu.slice(0, 300) });
    fs.appendFileSync(logFile + '.bodies', JSON.stringify({ t: new Date().toISOString(), url, body }) + '\n');
    const go = () => (body && body.stream ? sse(res, body.model, p) : json(res, (body || {}).model, p));
    return p.delay ? setTimeout(go, p.delay) : go();
  }
  if (/^\/api\/eval(-authed)?\//.test(url)) {
    const features = Object.fromEntries(Object.entries(FLAGS).map(([k, v]) => [k, { defaultValue: v }]));
    log({ ...base, reply: 'growthbook', flags: FLAGS, attributes: body && body.attributes ? Object.keys(body.attributes) : null });
    return send(res, 200, { features, experiments: [] });
  }
  if (url.startsWith('/api/oauth/profile')) { log({ ...base, reply: 'profile' }); return send(res, 200, { account: { ...ACCOUNT, has_claude_max: true, has_claude_pro: false, created_at: '2025-01-01T00:00:00Z' }, organization: ORG }); }
  if (url.startsWith('/api/oauth/claude_cli/roles')) { log({ ...base, reply: 'roles' }); return send(res, 200, { organization_role: 'admin', workspace_role: null, organization_name: ORG.name }); }
  if (url.startsWith('/api/event_logging/')) { log({ ...base, reply: 'ok', bytes: raw.length }); return send(res, 200, {}); }
  log({ ...base, status: 404, bodyHead: raw.slice(0, 300) });
  send(res, 404, { type: 'error', error: { type: 'not_found_error', message: 'rt-2189 intercept: no such route' } });
}
const tlsServer = https.createServer({ cert: fs.readFileSync(certFile), key: fs.readFileSync(keyFile) }, (req, res) => {
  let raw = '';
  req.on('data', (c) => (raw += c));
  req.on('end', () => { try { route(req, res, raw); } catch (e) { log({ error: String(e && e.stack || e) }); try { send(res, 500, {}); } catch {} } });
});
tlsServer.on('tlsClientError', (e, s) => log({ tlsClientError: String(e && e.message || e), servername: s && s.servername }));
const proxy = http.createServer((req, res) => { log({ refused: 'plain-http', method: req.method, url: req.url }); res.writeHead(403); res.end('refused'); });
proxy.on('connect', (req, sock, head) => {
  const [host, port] = String(req.url).split(':');
  if (!ANSWERED.has(host) || port !== '443') { log({ refused: 'connect', target: req.url }); sock.end('HTTP/1.1 403 Forbidden\r\n\r\n'); return; }
  log({ connect: req.url });
  sock.write('HTTP/1.1 200 Connection Established\r\n\r\n');
  if (head && head.length) sock.unshift(head);
  sock.on('error', () => {});
  tlsServer.emit('connection', sock);
});
proxy.listen(Number(process.env.RT2189_PORT || 0), '127.0.0.1', () => { fs.writeFileSync(portFile, String(proxy.address().port)); log({ listening: proxy.address().port, flags: FLAGS }); });
