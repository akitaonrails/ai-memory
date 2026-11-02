#!/usr/bin/env node
// Real Chromium + the built Rust server. No npm dependencies or DOM substitutes.
import assert from 'node:assert/strict';
import {spawn, spawnSync} from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const binary = path.resolve(process.env.AI_MEMORY_BIN || path.join(root, 'target/debug/ai-memory'));
const artifacts = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), 'ai-memory-web-e2e-'));
fs.chmodSync(artifacts, 0o700);
const token = 'FAKEfake0123456789WebDashboardRoot';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
function contrast(a, b) {
  const luminance = color => color.match(/[\d.]+/g).slice(0, 3).map(Number).map(value => {
    const channel = value / 255;return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  }).reduce((sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index], 0);
  const values = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (values[0] + 0.05) / (values[1] + 0.05);
}
const mutation = process.argv.find(arg => arg.startsWith('--mutation='))?.split('=')[1];
const selectedCase = process.argv.find(arg => arg.startsWith('--case='))?.split('=')[1];
const mutations = {
  'all-panels-visible': {test: 'tabs', from: 'panel.hidden=!selected', to: 'panel.hidden=false'},
  'ignore-recipient-workspace': {test: 'recipients', from: 'option.dataset.workspace===scope.dataset.workspace&&', to: ''},
  'ignore-recipient-project': {test: 'recipients', from: 'option.dataset.project===scope.dataset.project', to: 'true'},
  'retain-write-token': {test: 'delivery', from: 'if(token)token.value=""', to: 'if(token)token.value=token.value'},
  'disable-live-refresh': {test: 'live-usage', from: 'timer=setTimeout(refresh,10000);', to: 'timer=0;'},
  'disable-session-live-refresh': {test: 'session-live-usage', from: 'timer=setTimeout(refresh,10000);', to: 'timer=0;'},
  'drop-usage-query': {test: 'call-history', from: 'new FormData(form).forEach(function(value,key){url.searchParams.set(key,value);});', to: 'void form;'},
  'forget-theme': {test: 'themes', from: 'localStorage.setItem(key,selected)', to: 'void selected'},
  'steal-browser-shortcut': {test: 'tabs', from: 'if(event.altKey||event.ctrlKey||event.metaKey)return;', to: ''},
  'enable-send-while-pending': {test: 'delivery', from: 'button.disabled=sending||!toAgent', to: 'button.disabled=!toAgent'},
  'enable-send-for-empty-project-after-request': {test: 'delivery', from: 'sending=false;updateSendButton();result.focus()', to: 'sending=false;button.disabled=false;result.focus()'},
  'disable-release-auto-submit': {test: 'documentation', from: 'releaseSelect.form.requestSubmit();', to: 'void releaseSelect;'},
  'restore-stale-release-on-history': {test: 'documentation', from: 'releaseSelect.form.reset();', to: 'void releaseSelect;'},
  'show-false-inbox-alert': {test: 'mailbox-context', from: 'Inbox <span id="inbox-pending-count"', to: 'Inbox <small>NEW MESSAGE</small> <span id="inbox-pending-count"'},
  'ignore-mailbox-workspace': {test: 'mailbox-context', from: 'url.searchParams.set("workspace",o.dataset.workspace);', to: 'url.searchParams.set("workspace","team");'},
  'disable-inbox-notifications': {test: 'notifications', from: '  refreshInboxNotifications();\n})();', to: '  void refreshInboxNotifications;\n})();'},
  'ignore-notification-workspace': {test: 'notifications', from: 'url.searchParams.set("workspace",project.workspace);', to: 'url.searchParams.set("workspace","team");'},
  'skip-message-edit-hold': {test: 'message-editing', from: 'var data=await call(id,"edit");', to: 'var data={subject:row.dataset.subject||"",body:row.querySelector(".message-body").textContent};'},
  'lose-saved-message-text': {test: 'message-editing', from: 'subject:subjectInput.value,body:bodyInput.value', to: 'subject:subjectInput.value,body:"Lost edited request."'},
  'save-discarded-message-text': {test: 'message-editing', from: 'await call(id,"release");', to: 'await call(id,"save",{subject:"Discard mutation",body:"Discard mutation"});'},
};
assert(!mutation || mutations[mutation], 'Unknown mutation');
const report = {mutation: mutation || null, mutationApplied: 0, cases: []};
const children = [];
let cdp;

function chromePath() {
  if (process.env.AI_MEMORY_BROWSER) return process.env.AI_MEMORY_BROWSER;
  const cache = process.env.PLAYWRIGHT_BROWSERS_PATH || path.join(os.homedir(), '.cache/ms-playwright');
  if (fs.existsSync(cache)) {
    const versions = fs.readdirSync(cache).filter(name => /^chromium_headless_shell-\d+$/.test(name))
      .sort((a, b) => Number(b.split('-').at(-1)) - Number(a.split('-').at(-1)));
    for (const version of versions) {
      const executable = path.join(cache, version, 'chrome-headless-shell-linux64/chrome-headless-shell');
      if (fs.existsSync(executable)) return executable;
    }
  }
  throw Error('Chromium required: set AI_MEMORY_BROWSER or install the pinned browser from docs/testing-dashboard.md');
}

function launch(executable, args, env, log) {
  const fd = fs.openSync(path.join(artifacts, log), 'w', 0o600);
  try {
    const child = spawn(executable, args, {cwd: artifacts, env, stdio: ['ignore', fd, fd]});
    child.completion = new Promise(resolve => {
      child.once('error', error => resolve({error: error.message}));
      child.once('exit', code => resolve({code}));
    });
    children.push(child);
    return child;
  } finally { fs.closeSync(fd); }
}

async function until(predicate, label, timeout = 12000) {
  const deadline = Date.now() + timeout;
  do {
    if (await predicate()) return;
    await delay(50);
  } while (Date.now() < deadline);
  assert.fail(label);
}

// Values interpolated into code sent to Runtime.evaluate: escape the characters
// JSON leaves raw that can end or re-parse a script, not only quotes.
const literal = value => JSON.stringify(value).replace(/[<>\/\u2028\u2029]/g, c => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'));

class Cdp {
  constructor(socket) {
    this.socket = socket; this.next = 0; this.pending = new Map(); this.listeners = new Map(); this.errors = [];
    socket.onmessage = event => {
      const message = JSON.parse(event.data);
      if (message.id) {
        const pending = this.pending.get(message.id);
        if (!pending) return;
        this.pending.delete(message.id); clearTimeout(pending.timer);
        if (message.error) pending.reject(Error(message.error.message)); else pending.resolve(message.result);
      } else {
        if (message.method === 'Runtime.exceptionThrown') this.errors.push(message.params.exceptionDetails.text);
        const listener = this.listeners.get(message.method);
        if (typeof listener === 'function') Promise.resolve(listener(message)).catch(error => this.errors.push(error.message));
      }
    };
    socket.onclose = () => {
      for (const pending of this.pending.values()) {clearTimeout(pending.timer);pending.reject(Error('Chromium disconnected'));}
      this.pending.clear();
    };
  }
  send(method, params = {}, sessionId = this.sessionId) {
    return new Promise((resolve, reject) => {
      const id = ++this.next;
      const timer = setTimeout(() => {this.pending.delete(id);reject(Error('CDP timeout: ' + method));}, 10000);
      this.pending.set(id, {resolve, reject, timer});
      this.socket.send(JSON.stringify({id, method, params, ...(sessionId ? {sessionId} : {})}));
    });
  }
  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', {expression, returnByValue: true, awaitPromise: true});
    if (result.exceptionDetails) throw Error(result.exceptionDetails.text);
    return result.result.value;
  }
  async goto(url, selector, expected = url) {
    await this.send('Page.navigate', {url});
    await until(() => this.evaluate(`location.href===${literal(expected)}&&document.readyState==='complete'&&!!document.querySelector(${literal(selector)})`), 'Page did not load: ' + selector);
  }
  async reload() {
    await this.evaluate('window.reloadMarker=true');await this.send('Page.reload');
    await until(() => this.evaluate('!window.reloadMarker&&document.readyState==="complete"'), 'Page did not reload');
  }
  async key(key) {
    await this.send('Input.dispatchKeyEvent', {type: 'keyDown', key});
    await this.send('Input.dispatchKeyEvent', {type: 'keyUp', key});
  }
}

async function main() {
  assert(fs.existsSync(binary), 'Build ai-memory first or set AI_MEMORY_BIN');
  const browserBinary = chromePath();
  const portSocket = net.createServer();await new Promise(resolve => portSocket.listen(0, '127.0.0.1', resolve));
  const port = portSocket.address().port;await new Promise(resolve => portSocket.close(resolve));
  const base = `http://127.0.0.1:${port}`;
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('AI_MEMORY_')));
  env.AI_MEMORY_AUTH_TOKEN = token;
  const server = launch(binary, ['--data-dir', path.join(artifacts, 'server'), 'serve', '--transport', 'http', '--bind', `127.0.0.1:${port}`, '--enable-web', '--no-watcher', '--workspace', 'team', '--project', 'app'], env, 'server.log');
  const request = async (route, payload) => {
    const response = await fetch(base + route, {method: payload === undefined ? 'GET' : 'POST', headers: {Authorization: 'Bearer ' + token, 'Content-Type': 'application/json', 'X-Ai-Memory-UI': '1'}, ...(payload === undefined ? {} : {body: JSON.stringify(payload)}), signal: AbortSignal.timeout(15000)});
    assert(response.ok, `Fixture request ${route.split('?')[0]} returned ${response.status}`);
    return response;
  };
  await until(async () => {
    assert(server.exitCode === null, 'Isolated server exited');
    try {return (await fetch(base + '/web/', {headers: {Authorization: 'Bearer ' + token}})).ok;} catch {return false;}
  }, 'Isolated server failed to start');
  const sessions = new Map();
  for (const [workspace, project, agent] of [['team','app','codex'], ['team','app','claude-code'], ['team','other','claude-code'], ['other','app','gemini-cli']]) {
    const id = crypto.randomUUID();sessions.set(workspace + '/' + project + '/' + agent, id);
    await request('/hook?' + new URLSearchParams({workspace, project, agent, event: 'session-start'}), {session_id: id, cwd: path.join(artifacts, workspace, project)});
  }
  for (const project of ['app', 'empty']) await request('/admin/write-page', {workspace: 'team', project, path: 'concepts/fixture.md', title: 'Synthetic knowledge', body: '# Synthetic knowledge\n\nPublic fixture only.'});
  const id = sessions.get('team/app/codex');
  const calls = Array.from({length: 5}, (_, i) => ({call_id: 'response-' + i, input_tokens: i === 4 ? 150000 : 1000, output_tokens: i === 3 ? 25000 : 100, cache_read_tokens: 200, cache_write_tokens: 10, model: 'codex-fixture'}));
  const usage = async value => request('/hook?' + new URLSearchParams({workspace: 'team', project: 'app', agent: 'codex', event: 'post-tool-use'}), {session_id: id, tool_name: 'Bash', tool_input: {command: 'echo fixture'}, tool_response: 'fixture', _ai_memory_usage_calls: value});
  await usage(calls);await usage(calls);
  await until(async () => {
    const response = await request('/web/w/team/app');return (await response.text()).includes('Captured calls</span><strong>5</strong>');
  }, 'Hook reports never reached the real server');
  const profile = path.join(artifacts, 'chromium');
  launch(browserBinary, ['--headless', '--no-sandbox', '--disable-dev-shm-usage', '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank'], env, 'browser.log');
  const active = path.join(profile, 'DevToolsActivePort');await until(() => fs.existsSync(active), 'Chromium did not start');
  const debugPort = fs.readFileSync(active, 'utf8').split('\n')[0];
  const version = await (await fetch(`http://127.0.0.1:${debugPort}/json/version`)).json();
  const socket = new WebSocket(version.webSocketDebuggerUrl);await new Promise((resolve, reject) => {socket.onopen = resolve;socket.onerror = reject;});
  cdp = new Cdp(socket);
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  cdp.sessionId = (await cdp.send('Target.attachToTarget', {targetId, flatten: true})).sessionId;
  await cdp.send('Page.enable');await cdp.send('Runtime.enable');await cdp.send('Network.enable');
  await cdp.send('Network.setCacheDisabled', {cacheDisabled: true});
  await cdp.send('Network.setCookie', {name: 'ai_memory_auth', value: token, url: base, httpOnly: true, sameSite: 'Strict'});
  await cdp.send('Emulation.setDeviceMetricsOverride', {width: 1440, height: 1100, deviceScaleFactor: 1, mobile: false});
  {
    const change = mutation && mutations[mutation];
    const paused = async message => {
      const p = message.params;
      if (cdp.notificationStatus && p.responseStatusCode && new URL(p.request.url).pathname === '/web/inbox/notifications') {
        await cdp.send('Fetch.fulfillRequest', {requestId:p.requestId,responseCode:cdp.notificationStatus,responseHeaders:[{name:'Content-Type',value:'application/json'}],body:Buffer.from('{}').toString('base64')});return;
      }
      if (cdp.delayMessages && !p.responseStatusCode && new URL(p.request.url).pathname === '/web/messages') {
        cdp.pausedMessage = p.requestId;return;
      }
      if (change && p.responseStatusCode === 200 && p.responseHeaders?.some(h => h.name.toLowerCase() === 'content-type' && h.value.includes('text/html'))) {
        const body = await cdp.send('Fetch.getResponseBody', {requestId: p.requestId});
        let html = body.base64Encoded ? Buffer.from(body.body, 'base64').toString() : body.body;
        if (html.includes(change.from)) {html = html.split(change.from).join(change.to);report.mutationApplied++;}
        await cdp.send('Fetch.fulfillRequest', {requestId: p.requestId, responseCode: 200, responseHeaders: p.responseHeaders.filter(h => !['content-length','content-encoding'].includes(h.name.toLowerCase())), body: Buffer.from(html).toString('base64')});
      } else await cdp.send('Fetch.continueRequest', {requestId: p.requestId});
    };
    // A Page.reload, or the notification poll's own 5s abort, can cancel a paused request before
    // this handler resumes it; Chromium then answers "Invalid InterceptionId." and nothing is left to resume.
    cdp.listeners.set('Fetch.requestPaused', message => paused(message).catch(error => {if (error.message !== 'Invalid InterceptionId.') throw error;}));
    await cdp.send('Fetch.enable', {patterns: [...(mutation ? [{urlPattern: '*', requestStage: 'Response'}] : [{urlPattern:base+'/web/inbox/notifications',requestStage:'Response'}]), {urlPattern: base + '/web/messages', requestStage: 'Request'}]});
  }
  const project = base + '/web/w/team/app';
  const tab = async name => {
    await cdp.evaluate(`document.querySelector('[data-project-tab="${name}"]').click()`);
    await selected(name);
  };
  const selected = async name => {
    assert.deepEqual(await cdp.evaluate(`Array.from(document.querySelectorAll('[role=tabpanel]')).filter(p=>getComputedStyle(p).display!=='none').map(p=>p.id)`), ['project-panel-' + name]);
    assert.equal(await cdp.evaluate(`document.querySelector('[role=tab][aria-selected=true]').dataset.projectTab`), name);
  };
  const changeScope = (field, workspace, project) => cdp.evaluate(`(()=>{const s=document.getElementById(${literal(field)});s.selectedIndex=Array.from(s.options).findIndex(o=>o.dataset.workspace===${literal(workspace)}&&o.dataset.project===${literal(project)});if(s.selectedIndex<0)throw Error('Scope missing');s.dispatchEvent(new Event('change',{bubbles:true}));})()`);
  const recipients = () => cdp.evaluate(`Array.from(document.getElementById('message-to').options).filter(o=>o.dataset.agent).map(o=>[o.dataset.workspace,o.dataset.project,o.dataset.agent])`);
  const chooseAgent = agent => cdp.evaluate(`(()=>{const s=document.getElementById('message-to');s.value=Array.from(s.options).find(o=>o.dataset.agent===${literal(agent)}).value;})()`);
  const cases = {
    async notifications() {
      await cdp.goto(base+'/web/docs','.docs-search');
      await until(()=>cdp.evaluate('document.getElementById("inbox-notification-status").textContent==="No messages waiting for agents."'),'Initial notification check failed');
      assert(await cdp.evaluate('getComputedStyle(document.getElementById("inbox-pending-count")).display==="none"'));
      const anonymous=await fetch(base+'/web/inbox/notifications',{headers:{Accept:'application/json'},redirect:'manual'});assert.equal(anonymous.status,401);
      const sent=await (await request('/web/messages',{from_workspace:'team',from_project:'other',to_workspace:'other',to_project:'app',to_agent:'gemini-cli',subject:'Notification fixture',body:'Synthetic pending message.'})).json();
      await until(()=>cdp.evaluate('!document.getElementById("inbox-pending-count").hidden&&document.getElementById("inbox-pending-count").textContent==="1"'),'New message never reached the sidebar without reload',18000);
      assert.equal(await cdp.evaluate('location.pathname'),'/web/docs');
      const href=await cdp.evaluate('document.querySelector("[data-notification-project]").href');
      assert.equal(new URL(href).searchParams.get('workspace'),'other');assert.equal(new URL(href).searchParams.get('project'),'app');assert.equal(new URL(href).searchParams.get('state'),'pending');
      assert(await cdp.evaluate('document.getElementById("inbox-notifications").textContent.includes("other / app")'));
      cdp.notificationStatus=403;await cdp.evaluate('document.dispatchEvent(new Event("visibilitychange"))');
      await until(()=>cdp.evaluate('document.getElementById("inbox-pending-count").hidden&&document.getElementById("inbox-notification-projects").children.length===0'),'Authorization loss must clear stale counts and projects');
      cdp.notificationStatus=0;await cdp.evaluate('document.dispatchEvent(new Event("visibilitychange"))');
      await until(()=>cdp.evaluate('!document.getElementById("inbox-pending-count").hidden'),'Notifications did not recover');
      await cdp.goto(href,'.mail-row');assert.equal(await cdp.evaluate('document.querySelector(".page-heading strong").textContent'),'other / app');
      assert(await cdp.evaluate('document.querySelector(".mail-row").textContent.includes("Notification fixture")'));
      const popped=spawnSync(binary,['message','pop','--workspace','other','--project','app','--agent','gemini-cli','--id',sent.message_id,'--json'],{env:{...env,AI_MEMORY_SERVER_URL:base,AI_MEMORY_DATA_DIR:path.join(artifacts,'client')},encoding:'utf8',timeout:15000});
      assert.equal(popped.status,0);assert.equal(JSON.parse(popped.stdout).message.id,sent.message_id);
      await until(()=>cdp.evaluate('document.getElementById("inbox-pending-count").hidden&&document.getElementById("inbox-notifications").hidden'),'Collected mail must clear the notification without reload',18000);
      await cdp.evaluate('document.getElementById("mail-state").value="claimed";document.getElementById("mail-state").dispatchEvent(new Event("change",{bubbles:true}))');
      await until(()=>cdp.evaluate('new URL(location.href).searchParams.get("state")==="claimed"&&!!document.querySelector(".mail-row[data-state=claimed]")'),'State selector did not load collected history');
      assert.equal(new URL(await cdp.evaluate('document.querySelector(".segmented-tabs a[aria-current]").href')).searchParams.get('state'),'claimed');
      await cdp.goto(base+'/web/docs','.docs-search');await cdp.reload();
      await until(()=>cdp.evaluate('document.getElementById("inbox-notification-status").textContent==="No messages waiting for agents."'),'Notifications did not survive navigation and reload');
    },
    async 'resumed-session'() {
      const resumedId=crypto.randomUUID(),endpoint='/api/v1/workspaces/team/projects/app/sessions';
      const capture=event=>request('/hook?'+new URLSearchParams({workspace:'team',project:'app',agent:'codex',event}),{session_id:resumedId,cwd:path.join(artifacts,'team/app'),tool_name:'Bash',tool_input:{command:'echo synthetic-resume'},tool_response:'synthetic'});
      const recorded=async includeOpen=>(await (await request(endpoint+'?include_open='+includeOpen)).json()).sessions.find(s=>s.session_id===resumedId);
      await capture('session-start');await capture('post-tool-use');await capture('session-end');
      await until(async()=>!!(await recorded(false))?.ended_at,'Session end was not recorded');
      await capture('post-tool-use');
      await until(async()=>{const row=await recorded(true);return !!row&&!row.ended_at;},'New scoped work must reopen the read view');
      assert.equal(await recorded(false),undefined,'Completed listing must exclude resumed work before pagination');
      await cdp.goto(project+'/s/'+resumedId,'#scrubber');assert(await cdp.evaluate('document.querySelector("main").textContent.includes("still open")'));
      const foreign=await fetch(base+'/web/w/team/other/s/'+resumedId,{headers:{Authorization:'Bearer '+token}});assert.equal(foreign.status,404);
      await capture('session-end');await until(async()=>!!(await recorded(false))?.ended_at,'A later SessionEnd must close resumed work');
      await cdp.reload();assert.equal(await cdp.evaluate('document.querySelector("main").textContent.includes("still open")'),false);
    },
    async tabs() {
      await cdp.goto(project, '#project-panel-overview');await selected('overview');
      for (const name of ['agents','tokens','sessions','knowledge']) {await tab(name);assert.equal(new URL(await cdp.evaluate('location.href')).searchParams.get('tab'), name);}
      await cdp.evaluate('history.back()');await until(() => cdp.evaluate(`document.querySelector('[data-project-tab=sessions]').getAttribute('aria-selected')==='true'`), 'Back did not restore Sessions');await selected('sessions');
      await cdp.evaluate('history.forward()');await until(() => cdp.evaluate(`document.querySelector('[data-project-tab=knowledge]').getAttribute('aria-selected')==='true'`), 'Forward did not restore Knowledge');
      for (const [key, expected] of [['ArrowRight','overview'], ['ArrowLeft','knowledge'], ['Home','overview'], ['End','knowledge']]) {await cdp.key(key);await selected(expected);}
      await cdp.reload();await selected('knowledge');
      await cdp.evaluate(`document.querySelector('[data-project-tab=knowledge]').focus();document.addEventListener('keydown',event=>{window.shortcutCanceled=event.defaultPrevented;},{once:true});`);
      await cdp.send('Input.dispatchKeyEvent', {type: 'keyDown', key: 'ArrowLeft', modifiers: 1});
      await cdp.send('Input.dispatchKeyEvent', {type: 'keyUp', key: 'ArrowLeft', modifiers: 1});
      assert.equal(await cdp.evaluate('window.shortcutCanceled'), false, 'Tabs stole a browser shortcut');
      await cdp.goto(project + '#token-usage', '#token-usage', project + '?tab=tokens');await selected('tokens');
      await cdp.send('Accessibility.enable');const tree = await cdp.send('Accessibility.getFullAXTree');
      assert.equal(tree.nodes.filter(n => !n.ignored && n.role?.value === 'tabpanel').length, 1);
    },
    async recipients() {
      await cdp.goto(base + '/web/inbox?workspace=team&project=app&agent=codex', '#message-to');
      assert.equal((await recipients()).length, 2);
      await changeScope('message-from', 'team', 'other');
      assert.deepEqual(await recipients(), [['team','other','claude-code']]);
      assert.equal(await cdp.evaluate('document.getElementById("message-to").value'), '');
      await changeScope('message-to-project', 'other', 'app');
      assert.deepEqual(await recipients(), [['other','app','gemini-cli']]);
      await changeScope('message-from', 'team', 'empty');
      assert.deepEqual(await recipients(), []);
      assert(await cdp.evaluate(`document.getElementById('message-to').disabled&&document.querySelector('#message-composer button[type=submit]').disabled`));
      await changeScope('message-to-project', 'team', 'app');assert.equal((await recipients()).length, 2);
      assert(await cdp.evaluate(`!document.getElementById('message-to').disabled&&!document.querySelector('#message-composer button[type=submit]').disabled`));
    },
    async delivery() {
      await cdp.goto(base + '/web/inbox?workspace=team&project=other', '#message-composer');
      await changeScope('message-to-project', 'other', 'app');await chooseAgent('gemini-cli');
      const storage = await cdp.evaluate('JSON.stringify({local:Object.entries(localStorage),session:Object.entries(sessionStorage)})');
      assert.equal(await cdp.evaluate(`fetch(new URL('messages',document.baseURI),{method:'POST',headers:{'Content-Type':'application/json','X-Ai-Memory-UI':'1'},body:'{}'}).then(r=>r.status)`), 401);
      await cdp.evaluate(`document.getElementById('message-access-token').value='FAKEfake0123456789InvalidWrite';document.getElementById('message-body').value='Synthetic request';document.querySelector('#message-composer button[type=submit]').click()`);
      await until(() => cdp.evaluate(`document.getElementById('message-result').classList.contains('send-error')`), 'Invalid credentials were not rejected');
      assert.equal(await cdp.evaluate('document.getElementById("message-access-token").value'), '');
      assert.equal(await cdp.evaluate('JSON.stringify({local:Object.entries(localStorage),session:Object.entries(sessionStorage)})'), storage);
      assert.equal(await cdp.evaluate('document.getElementById("message-body").value'), 'Synthetic request');
      cdp.delayMessages = true;
      await cdp.evaluate(`document.getElementById('message-access-token').value=${literal(token)};document.getElementById('message-subject').value='Review <script>window.fixtureXss=true</script>';document.querySelector('#message-composer button[type=submit]').click()`);
      await until(() => !!cdp.pausedMessage, 'Message request was not issued');
      assert(await cdp.evaluate(`document.querySelector('#message-composer button[type=submit]').disabled`), 'Sending must remain disabled during a request');
      await changeScope('message-to-project', 'team', 'app');
      assert(await cdp.evaluate(`document.querySelector('#message-composer button[type=submit]').disabled`), 'Changing project must not allow duplicate sends');
      await changeScope('message-to-project', 'team', 'empty');
      cdp.delayMessages = false;
      await cdp.send('Fetch.continueRequest', {requestId: cdp.pausedMessage});
      await until(() => cdp.evaluate(`document.getElementById('message-result').classList.contains('send-success')`), 'Authorized message was not queued');
      assert.equal(await cdp.evaluate('document.getElementById("message-access-token").value'), '');
      assert.equal(await cdp.evaluate('JSON.stringify({local:Object.entries(localStorage),session:Object.entries(sessionStorage)})'), storage);
      assert.equal(await cdp.evaluate('document.getElementById("message-body").value'), '');
      assert(await cdp.evaluate(`document.querySelector('#message-composer button[type=submit]').disabled`), 'An empty destination must remain disabled after completion');
      const messageId = (await cdp.evaluate('document.getElementById("message-result").textContent')).match(/[0-9a-f]{8}-[0-9a-f-]{27,}/i)?.[0];assert(messageId);
      const sent = await cdp.evaluate('document.querySelector("#message-result a").href');
      await cdp.goto(sent, '.mail-row');assert(await cdp.evaluate(`document.querySelector('.mail-row').textContent.includes('Review <script>window.fixtureXss=true</script>')&&!window.fixtureXss`));
      const pop = agent => {
        const result = spawnSync(binary, ['message','pop','--workspace','other','--project','app','--agent',agent,'--id',messageId,'--json'], {env: {...env, AI_MEMORY_SERVER_URL: base, AI_MEMORY_DATA_DIR: path.join(artifacts, 'client')}, encoding: 'utf8', timeout: 15000});
        assert.equal(result.status, 0, 'Native message pop failed');return JSON.parse(result.stdout).message;
      };
      assert.equal(pop('codex'), null);assert.equal(pop('gemini-cli').id, messageId);assert.equal(pop('gemini-cli'), null);
    },
    async 'message-editing'() {
      const inbox = base + '/web/inbox?workspace=team&project=app';
      const row = messageId => '.mail-row[data-message-id="' + messageId + '"]';
      const queue = async (subject, body) => (await (await request('/web/messages', {from_workspace:'team',from_project:'other',to_workspace:'team',to_project:'app',to_agent:'codex',subject,body})).json()).message_id;
      const pop = messageId => {
        const result = spawnSync(binary, ['message','pop','--workspace','team','--project','app','--agent','codex','--id',messageId,'--json'], {env:{...env,AI_MEMORY_SERVER_URL:base,AI_MEMORY_DATA_DIR:path.join(artifacts,'edit-client')},encoding:'utf8',timeout:15000});
        assert.equal(result.status, 0, 'Native claim failed: ' + result.stderr);return JSON.parse(result.stdout).message;
      };
      const open = async (messageId, action) => {
        await cdp.evaluate(`(()=>{const row=document.querySelector(${literal(row(messageId))});if(!row.open)row.querySelector('summary').click();row.querySelector('[data-message-action="${action}"]').click();})()`);
        await until(() => cdp.evaluate(`!!document.querySelector(${literal(row(messageId) + ' .mail-editor')})`), 'Message editor did not open');
      };
      const fill = async (messageId, subject, body) => cdp.evaluate(`document.getElementById(${literal('edit-subject-' + messageId)}).value=${literal(subject)};document.getElementById(${literal('edit-body-' + messageId)}).value=${literal(body)}`);
      const requeued = messageId => until(() => cdp.evaluate(`document.readyState==='complete'&&!!document.querySelector(${literal(row(messageId) + '[data-state=pending]')})`), 'Edited message did not return to the queue');
      const savedId = await queue('Original subject', 'Original request.');
      await cdp.goto(inbox, row(savedId));
      assert.equal(await cdp.evaluate(`document.querySelector(${literal(row(savedId) + ' [data-message-action]')})`), null, 'Legacy read-only login must not offer editing');
      assert.equal(await cdp.evaluate(`fetch(new URL('messages/'+${literal(savedId)}+'/edit',document.baseURI),{method:'POST',headers:{'Content-Type':'application/json','X-Ai-Memory-UI':'1'},body:JSON.stringify({workspace:'team',project:'app'})}).then(r=>r.status)`), 401, 'Legacy read-only credentials must not hold mail through the edit route');
      // The normal suite uses the legacy read-only cookie. This case supplies
      // an actual root bearer to exercise writes without persisting credentials.
      await cdp.send('Network.setExtraHTTPHeaders', {headers:{Authorization:'Bearer ' + token}});
      try {
        await cdp.reload();await open(savedId, 'begin');
        assert.equal(await cdp.evaluate(`document.getElementById(${literal('edit-body-' + savedId)}).value`), 'Original request.');
        assert.equal(pop(savedId), null, 'Opening the editor must hold the real queued message, even for a claim by id');
        const editedBody = 'Updated <b>literal text</b> for the agent.';
        await fill(savedId, 'Updated subject', editedBody);
        await cdp.evaluate(`document.querySelector(${literal(row(savedId) + ' .mail-editor button[type=submit]')}).click()`);
        await requeued(savedId);
        assert(await cdp.evaluate(`document.querySelector(${literal(row(savedId) + ' .message-body')}).textContent===${literal(editedBody)}&&!document.querySelector(${literal(row(savedId) + ' .message-body b')})`), 'Saved text must render escaped');
        const saved = pop(savedId);assert.equal(saved.id, savedId);assert.equal(saved.subject, 'Updated subject');assert.equal(saved.body, editedBody);assert.equal(pop(savedId), null);

        const discardId = await queue('Keep this subject', 'Keep this request.');
        await cdp.goto(inbox, row(discardId));await open(discardId, 'begin');
        assert.equal(pop(discardId), null);
        await fill(discardId, 'Unsaved subject', 'Unsaved request.');
        await cdp.evaluate(`document.querySelector(${literal(row(discardId) + ' .mail-editor button[type=button]')}).click()`);
        await requeued(discardId);
        const discarded = pop(discardId);assert.equal(discarded.id, discardId);assert.equal(discarded.subject, 'Keep this subject');assert.equal(discarded.body, 'Keep this request.');

        const resumedId = await queue('Recover this edit', 'Recover the original request.');
        await cdp.goto(inbox, row(resumedId));await open(resumedId, 'begin');
        await fill(resumedId, 'Unsaved before navigation', 'Unsaved before navigation.');
        await cdp.goto(base + '/web/docs', '.docs-search');await cdp.goto(inbox, '.held-note');
        assert(await cdp.evaluate('document.querySelector(".held-note").textContent.includes("1 held for editing")'));
        assert.equal(pop(resumedId), null, 'Leaving the editor must not release held mail');
        const heldHref = await cdp.evaluate('document.querySelector(".held-note a").href');
        assert.equal(new URL(heldHref).searchParams.get('state'), 'editing');
        await cdp.goto(heldHref, row(resumedId));await cdp.reload();await open(resumedId, 'resume');
        assert.equal(await cdp.evaluate(`document.getElementById(${literal('edit-body-' + resumedId)}).value`), 'Recover the original request.');
        await cdp.reload();
        await cdp.evaluate(`(()=>{const row=document.querySelector(${literal(row(resumedId))});row.querySelector('summary').click();row.querySelector('[data-message-action=release]').click();})()`);
        await until(() => cdp.evaluate(`document.readyState==='complete'&&!document.querySelector('.held-note')&&!document.querySelector(${literal(row(resumedId))})`), 'Discarding a recovered edit must clear the held list');
        assert.equal(pop(resumedId).body, 'Recover the original request.');

        const collectedId = await queue('Claim race', 'Collected before Edit.');
        await cdp.goto(inbox, row(collectedId));assert.equal(pop(collectedId).id, collectedId);
        await cdp.evaluate(`(()=>{const row=document.querySelector(${literal(row(collectedId))});row.querySelector('summary').click();row.querySelector('[data-message-action=begin]').click();})()`);
        await until(() => cdp.evaluate(`document.querySelector(${literal(row(collectedId) + ' .edit-error')})?.textContent.includes('already collected')`), 'A stale Edit must explain that the agent already collected the message');
        assert.equal(await cdp.evaluate(`document.querySelector(${literal(row(collectedId) + ' .mail-editor')})`), null);
        assert.equal(pop(collectedId), null, 'A refused edit must not requeue collected mail');
      } finally {await cdp.send('Network.setExtraHTTPHeaders', {headers:{}});}
    },
    async 'live-usage'() {
      await cdp.goto(project + '?tab=tokens', '#token-usage');await selected('tokens');
      assert.equal(await cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)'), 5);
      assert(await cdp.evaluate(`Array.from(document.querySelectorAll('[data-call-row]')).some(r=>r.textContent.includes('Above 3× average'))`));
      await cdp.evaluate(`document.getElementById('call-filter').value='codex-fixture';document.getElementById('call-filter').dispatchEvent(new Event('input',{bubbles:true}));document.getElementById('call-filter').focus()`);
      await until(()=>cdp.evaluate(`new URL(location.href).searchParams.get('calls_q')==='codex-fixture'&&!document.getElementById('call-query').hasAttribute('aria-busy')`),'Search must settle before testing the periodic refresh');
      const next = {...calls[0], call_id: 'response-live', input_tokens: 321};await usage([next]);await usage([next]);
      await until(() => cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)===6'), 'Live reports did not refresh exactly once', 18000);
      await selected('tokens');assert(await cdp.evaluate(`document.activeElement.id==='call-filter'&&document.getElementById('call-filter').value==='codex-fixture'`));
      await tab('agents');assert.equal(await cdp.evaluate(`Number(document.querySelector('[data-agent=codex] .agent-card-usage div:last-child strong').textContent)`), 6);
      await cdp.goto(project + '/s/' + id, '#token-usage');
      assert.equal(await cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)'), 6);
    },
    async 'capture-backlog'() {
      const cwd=path.join(artifacts,'backlog-repository'),client=path.join(artifacts,'backlog-client'),transcript=path.join(artifacts,'backlog.jsonl'),sid=crypto.randomUUID();
      fs.mkdirSync(cwd);fs.writeFileSync(path.join(cwd,'.ai-memory.toml'),'workspace = "backlog"\nproject = "capture"\n');
      fs.writeFileSync(transcript,Array.from({length:300},(_,n)=>JSON.stringify({type:'assistant',message:{id:'backlog-'+n,model:'backlog-model',usage:{input_tokens:100,output_tokens:20}}})+'\n').join(''));
      const options={cwd,env:{...env,AI_MEMORY_HOME:client},encoding:'utf8',timeout:15000};
      for(const expected of [256,300,300]) {
        const hook=spawnSync(binary,['--data-dir',client,'hook','--event','post-tool-use','--agent','claude-code','--server-url',base,'--auth-token',token],{...options,input:JSON.stringify({session_id:sid,cwd,transcript_path:transcript,tool_name:'Bash',tool_input:{command:'echo fixture'},tool_response:'fixture'})});
        assert.equal(hook.status,0,'Native catch-up hook failed: '+hook.stderr);
        const drain=spawnSync(binary,['--data-dir',client,'hook-drain'],options);assert.equal(drain.status,0,'Native catch-up drain failed: '+drain.stderr);
        await until(async()=> (await (await request('/web/w/backlog/capture')).text()).includes('Captured calls</span><strong>'+expected+'</strong>'),'Native backlog did not reach '+expected+' distinct stored responses');
      }
      await cdp.goto(base+'/web/w/backlog/capture?tab=tokens','#token-usage');
      assert.equal(await cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)'),300);
      assert(await cdp.evaluate('document.querySelector(".usage-pagination").textContent.includes("300 matching responses")'));
    },
    async 'session-live-usage'() {
      await cdp.goto(project+'/s/'+id,'#token-usage');
      const before=await cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)');
      await cdp.evaluate(`document.getElementById('call-filter').value='codex-fixture';document.getElementById('call-filter').dispatchEvent(new Event('input',{bubbles:true}));document.getElementById('call-filter').focus()`);
      await until(()=>cdp.evaluate(`new URL(location.href).searchParams.get('calls_q')==='codex-fixture'&&!document.getElementById('call-query').hasAttribute('aria-busy')`),'Session search must settle before the new response');
      await usage([{...calls[0],call_id:'session-live-response',input_tokens:123}]);
      await until(()=>cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)==='+ (before+1)), 'Session usage did not refresh without reload',18000);
      assert(await cdp.evaluate(`document.activeElement.id==='call-filter'&&document.getElementById('call-filter').value==='codex-fixture'`));
      assert(await cdp.evaluate(`Array.from(document.querySelectorAll('[data-call-row]')).some(r=>r.textContent.includes('session-live-response'))`));
    },
    async 'call-history'() {
      const workspace='history',name='app',sid=crypto.randomUUID(),href=base+'/web/w/history/app';
      const hook='/hook?'+new URLSearchParams({workspace,project:name,agent:'codex',event:'post-tool-use'});
      const publish=async rows=>request(hook,{session_id:sid,tool_name:'Bash',tool_input:{command:'echo synthetic'},tool_response:'synthetic',_ai_memory_usage_calls:rows.map(row=>({cache_read_tokens:0,cache_write_tokens:0,...row}))});
      await request('/hook?'+new URLSearchParams({workspace,project:name,agent:'codex',event:'session-start'}),{session_id:sid});
      await publish(Array.from({length:56},(_,n)=>({call_id:'bulk-'+n,input_tokens:1000+n,output_tokens:10,model:'bulk-model'})));
      await until(async()=> (await (await request('/web/w/history/app')).text()).includes('Captured calls</span><strong>56</strong>'),'History fixture did not settle');
      await publish([{call_id:'tiny-newest',input_tokens:1,output_tokens:1,model:'unique-tiny-model'}]);
      await until(async()=> (await (await request('/web/w/history/app')).text()).includes('Captured calls</span><strong>57</strong>'),'Small response did not settle');
      await cdp.goto(href+'?tab=tokens','#token-usage');await selected('tokens');
      assert.equal(await cdp.evaluate('document.querySelectorAll("[data-call-row]").length'),50);
      assert(!await cdp.evaluate('document.querySelector(".usage-results").textContent.includes("tiny-newest")'));
      await cdp.evaluate(`document.getElementById('call-sort').value='recent';document.getElementById('call-sort').dispatchEvent(new Event('change',{bubbles:true}))`);
      await until(()=>cdp.evaluate('document.querySelector("[data-call-row]")?.textContent.includes("tiny-newest")'),'Recent ordering omitted the small newest response');
      await cdp.evaluate(`document.getElementById('call-filter').value='unique-tiny-model';document.getElementById('call-filter').dispatchEvent(new Event('input',{bubbles:true}))`);
      await until(()=>cdp.evaluate('document.querySelectorAll("[data-call-row]").length===1&&document.querySelector("[data-call-row]")?.textContent.includes("tiny-newest")'),'Search did not cover calls outside the largest fifty');
      assert.equal(await cdp.evaluate('Number(document.querySelector(".usage-metrics strong").textContent)'),57);
      await cdp.evaluate(`document.getElementById('call-filter').value='';document.querySelector('#call-query').requestSubmit()`);
      await until(()=>cdp.evaluate('document.querySelectorAll("[data-call-row]").length===50&&!!document.querySelector("[data-call-page]")'),'Clearing search did not restore the page');
      await cdp.evaluate('document.querySelector("[data-call-page]").click()');
      await until(()=>cdp.evaluate('document.querySelectorAll("[data-call-row]").length===7&&new URL(location.href).searchParams.get("calls_offset")==="50"'),'Response pagination lost results');
      await cdp.reload();assert.equal(await cdp.evaluate('document.querySelectorAll("[data-call-row]").length'),7);
      assert.equal(await cdp.evaluate('document.getElementById("call-sort").value'),'recent');
      await cdp.goto(href+'/s/'+sid+'?calls_q=unique-tiny-model','#token-usage');
      assert.equal(await cdp.evaluate('document.querySelectorAll("[data-call-row]").length'),1);
      assert(await cdp.evaluate('document.querySelector("[data-call-row]").textContent.includes("tiny-newest")'));
    },
    async 'contextual-investigation'() {
      const sid=crypto.randomUUID(),href=base+'/web/w/investigation/app';
      const hook='/hook?'+new URLSearchParams({workspace:'investigation',project:'app',agent:'codex',event:'post-tool-use'});
      await request('/hook?'+new URLSearchParams({workspace:'investigation',project:'app',agent:'codex',event:'session-start'}),{session_id:sid});
      const rows=Array.from({length:57},(_,n)=>({call_id:'context-'+n,model:n===56?'rare-model':'bulk-model',input_tokens:20,output_tokens:2,cache_read_tokens:0,cache_write_tokens:0,context:{turn_id:'native-turn',response_id:'native-'+n,recorded_at:'2026-10-01T12:00:00Z',tools:[{id:'exec-1',name:'exec_command',relation:'turn',is_error:true}]}}));
      await request(hook,{session_id:sid,tool_name:'Bash',tool_input:{command:'echo synthetic'},tool_response:'synthetic',_ai_memory_usage_calls:rows});
      await until(async()=> (await (await request('/web/w/investigation/app')).text()).includes('Captured calls</span><strong>57</strong>'),'Context fixture never reached storage');
      await cdp.goto(href+'?tab=tokens','#token-usage');
      await cdp.evaluate(`document.querySelector('[name=calls_model]').value='rare-model';document.querySelector('[name=calls_agent]').value='codex';document.querySelector('[name=calls_session]').value=${literal(sid)};document.querySelector('[name=calls_from]').value='2026-10-01T12:00:00';document.querySelector('[name=calls_until]').value='2026-10-01T12:00:01';document.querySelector('[name=calls_tool]').value='exec_command';document.getElementById('call-query').requestSubmit()`);
      await until(()=>cdp.evaluate('document.querySelectorAll("[data-call-row]").length===1&&!document.querySelector("#call-query").hasAttribute("aria-busy")'),'Server investigation filters did not isolate the response outside the first fifty');
      assert(await cdp.evaluate('document.querySelector("[data-call-row]").textContent.includes("native-56")&&document.querySelector("[data-call-row]").textContent.includes("association")'));
      await cdp.reload();assert.equal(await cdp.evaluate('document.querySelectorAll("[data-call-row]").length'),1);
      await cdp.evaluate(`document.querySelector('[name=calls_model]').value='missing-model';document.querySelector('[name=calls_model]').dispatchEvent(new Event('change',{bubbles:true}))`);
      await until(()=>cdp.evaluate('document.querySelectorAll("[data-call-row]").length===0&&!document.querySelector("#call-query").hasAttribute("aria-busy")'),'A mismatched exact model must remove all rows');
      for(const width of [320,390]) {await cdp.send('Emulation.setDeviceMetricsOverride',{width,height:960,deviceScaleFactor:1,mobile:true});assert(await cdp.evaluate('document.documentElement.scrollWidth<=innerWidth'),'Investigation controls overflow the viewport');}
      await cdp.send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    },
    async 'feature-states'() {
      await cdp.goto(base+'/web/docs/features','.installation-status');
      assert(await cdp.evaluate(`document.querySelector('[data-feature="learning"]').textContent.includes("Unavailable without LLM")`));
      assert(await cdp.evaluate(`document.querySelector('[data-feature="external-clients"]').textContent.includes("Client setup unknown")`));
      assert(await cdp.evaluate(`document.querySelector('[data-feature="consolidation"]').textContent.includes("Next step:")`));
      assert.equal((await fetch(base+'/api/v1/docs/features/status',{redirect:'manual'})).status,401);
      const status=await (await request('/api/v1/docs/features/status')).json();assert.equal(status.features.find(f=>f.id==='embeddings').state,'Not configured');
    },
    async 'knowledge-evidence'() {
      const sid=sessions.get('team/app/codex');
      const rpc=async(body,session)=> {
        const response=await fetch(base+'/mcp',{method:'POST',headers:{Authorization:'Bearer '+token,'Content-Type':'application/json',Accept:'application/json, text/event-stream',...(session?{'Mcp-Session-Id':session}:{})},body:JSON.stringify(body),signal:AbortSignal.timeout(15000)});
        assert(response.ok,'MCP fixture returned '+response.status);
        const text=await response.text();const payload=text.trim().startsWith('{')?JSON.parse(text):JSON.parse(text.split('\n').find(line=>line.startsWith('data: ')).slice(6));return {payload,session:response.headers.get('Mcp-Session-Id')};
      };
      const initialized=await rpc({jsonrpc:'2.0',id:1,method:'initialize',params:{protocolVersion:'2025-03-26',capabilities:{},clientInfo:{name:'synthetic-dashboard-test',version:'1'}}});
      const written=await rpc({jsonrpc:'2.0',id:2,method:'tools/call',params:{name:'memory_write_page',arguments:{workspace:'team',project:'app',path:'concepts/evidence.md',body:'# Evidence fixture\n\nVerificationneedle source.',session_id:sid}}},initialized.session);
      assert(!written.payload.error&&!written.payload.result?.isError,'MCP evidence fixture failed: '+JSON.stringify(written.payload));
      await cdp.goto(base+'/web/search?workspace=team&project=app&q=Verificationneedle','[data-hit-path]');
      const href=await cdp.evaluate(`document.querySelector('[data-hit-path="concepts/evidence.md"] a').href`);assert.equal(new URL(href).searchParams.get('recall_q'),'Verificationneedle');
      await cdp.goto(href,'.knowledge-evidence');assert(await cdp.evaluate('document.querySelector(".knowledge-evidence").textContent.includes("Session '+sid+'")'));
      await cdp.evaluate('document.querySelector(".knowledge-evidence a.secondary-button").click()');await until(()=>cdp.evaluate('!!document.querySelector("[data-page-recall-trace]")'),'Page did not recompute its actual recall trace');
      assert(await cdp.evaluate('document.querySelector(".knowledge-evidence").textContent.includes("Recomputed now")'));
      for(const width of [320,390]) {await cdp.send('Emulation.setDeviceMetricsOverride',{width,height:960,deviceScaleFactor:1,mobile:true});assert(await cdp.evaluate('document.documentElement.scrollWidth<=innerWidth'),'Knowledge evidence overflows the viewport');}
      await cdp.send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    },
    async themes() {
      await cdp.goto(project, '#project-panel-overview');
      await cdp.send('Emulation.setEmulatedMedia', {features: [{name: 'prefers-color-scheme', value: 'light'}]});
      await cdp.evaluate(`document.querySelector('[data-theme-choice=dark]').click()`);await cdp.reload();
      assert.equal(await cdp.evaluate('document.documentElement.classList.contains("dark")'), true);
      await cdp.evaluate(`document.querySelector('[data-theme-choice=light]').click()`);
      assert.equal(await cdp.evaluate('document.documentElement.classList.contains("dark")'), false);
      await cdp.evaluate(`document.querySelector('[data-theme-choice=system]').click()`);
      await cdp.send('Emulation.setEmulatedMedia', {features: [{name: 'prefers-color-scheme', value: 'dark'}]});
      await until(() => cdp.evaluate('document.documentElement.classList.contains("dark")'), 'System theme did not follow OS');
      await cdp.evaluate(`document.querySelector('[data-theme-choice=light]').click()`);
      const colors = await cdp.evaluate(`({background:getComputedStyle(document.body).backgroundColor,foreground:getComputedStyle(document.body).color})`);
      assert(contrast(colors.background, colors.foreground) >= 4.5, 'Light body text is not readable');
      await cdp.evaluate(`document.querySelector('[data-theme-choice=dark]').click()`);
      assert.notEqual(await cdp.evaluate('getComputedStyle(document.body).backgroundColor'), colors.background);
      assert.notEqual(await cdp.evaluate('getComputedStyle(document.body).color'), colors.foreground);
      const dark = await cdp.evaluate(`({background:getComputedStyle(document.body).backgroundColor,foreground:getComputedStyle(document.body).color})`);
      assert(contrast(dark.background, dark.foreground) >= 4.5, 'Dark body text is not readable');
    },
    async documentation() {
      await cdp.goto(base + '/web/docs', '.docs-search');
      const anonymous = await fetch(base + '/api/v1/docs', {redirect: 'manual'});assert.equal(anonymous.status, 401);
      await cdp.evaluate(`document.getElementById('documentation-query').value='Token accounting';document.querySelector('.docs-search').requestSubmit()`);
      await until(() => cdp.evaluate(`new URL(location.href).searchParams.get('q')==='Token accounting'&&document.querySelector('.docs-card')?.textContent.includes('Features')`), 'Documentation search failed');
      const catalog = await (await request('/api/v1/docs')).json();assert(catalog.documents.length >= 28);
      for (const doc of catalog.documents) {
        const json = await (await request('/api/v1/docs/' + doc.id)).json();
        assert.equal(json.markdown, await (await request('/web/docs/' + doc.id + '.md')).text());
      }
      const release = await (await request('/api/v1/docs/changelog?release=3.2.2')).json();
      assert(release.markdown.startsWith('## [3.2.2]'));assert(!release.markdown.includes('## [3.2.1]'));
      await cdp.goto(base + '/web/docs/changelog', '#release-version');
      assert.equal(await cdp.evaluate('document.querySelector(".docs-release button")'), null, 'JavaScript users should not need a submit button');
      const selectedRelease = async version => {
        await until(() => cdp.evaluate(`new URL(location.href).searchParams.get('release')===${literal(version)}&&document.readyState==='complete'&&document.getElementById('release-version')?.value===${literal(version)}&&document.querySelector('.docs-article h2')?.textContent.startsWith(${literal('[' + version + ']')})`), 'Release selection did not load ' + version);
        assert.equal(await cdp.evaluate('document.getElementById("release-version").value'), version);
        assert(await cdp.evaluate(`new URL(document.querySelector('.page-heading a').href).searchParams.get('release')===${literal(version)}&&Array.from(document.querySelectorAll('.docs-outline a')).every(a=>new URL(a.href).searchParams.get('release')===${literal(version)})`), 'Release links must match the displayed content');
      };
      for (const version of ['3.2.2', '3.2.0', 'Unreleased']) {
        await cdp.evaluate(`(()=>{const select=document.getElementById('release-version');select.value=${literal(version)};select.dispatchEvent(new Event('change',{bubbles:true}));})()`);
        await selectedRelease(version);
      }
      await cdp.reload();await selectedRelease('Unreleased');
      await cdp.evaluate('history.back()');await selectedRelease('3.2.0');
      await cdp.evaluate('history.forward()');await selectedRelease('Unreleased');
      await cdp.send('Emulation.setScriptExecutionDisabled', {value: true});
      try {
        await cdp.goto(base + '/web/docs/changelog?release=3.2.2', '.docs-release button');
        assert(await cdp.evaluate('getComputedStyle(document.querySelector(".docs-release button")).display!=="none"'), 'No-JavaScript release submission must remain available');
        await cdp.evaluate(`document.getElementById('release-version').value='3.2.0';document.querySelector('.docs-release button').click()`);
        await selectedRelease('3.2.0');
      } finally {await cdp.send('Emulation.setScriptExecutionDisabled', {value: false});}
      const missing = await fetch(base + '/web/docs/.env.md', {headers: {Authorization: 'Bearer ' + token}});assert.equal(missing.status, 404);
    },
    async 'mailbox-context'() {
      await cdp.goto(base + '/web/inbox?workspace=team&project=empty', '#mail-project');
      assert.equal(await cdp.evaluate('document.querySelector("[data-nav=inbox]").textContent.includes("NEW MESSAGE")'), false, 'Navigation must not show a fabricated new-message alert');
      assert.equal(await cdp.evaluate('document.querySelectorAll(".mailbox-link").length'), 0);
      assert(await cdp.evaluate('document.querySelector(".mail-overview").textContent.includes("No messages waiting")'));
      const ids = [];
      for (const [workspace, agent] of [['team','codex'], ['other','gemini-cli'], ['other','gemini-cli']]) {
        const sent = await request('/web/messages', {from_workspace:'team',from_project:'other',to_workspace:workspace,to_project:'app',to_agent:agent,subject:'Mailbox context fixture',body:'Synthetic context check.'});
        ids.push((await sent.json()).message_id);
      }
      await cdp.reload();
      const cards = await cdp.evaluate(`Array.from(document.querySelectorAll('.mailbox-link')).map(a=>({workspace:a.dataset.mailboxWorkspace,project:a.dataset.mailboxProject,href:a.href,text:a.textContent}))`);
      assert.equal(cards.length, 2);
      for (const [workspace, count] of [['team',1], ['other',2]]) {
        const card = cards.find(c => c.workspace === workspace && c.project === 'app');assert(card);
        assert(card.text.includes(count + ' queued'));assert.equal(new URL(card.href).searchParams.get('workspace'), workspace);assert.equal(new URL(card.href).searchParams.get('project'), 'app');
      }
      assert(await cdp.evaluate('document.querySelector(".mail-overview").textContent.includes("3 queued")'));
      assert.equal(await cdp.evaluate('document.querySelector(".page-heading strong").textContent'), 'team / empty');
      await changeScope('mail-project', 'other', 'app');
      await until(() => cdp.evaluate(`new URL(location.href).searchParams.get('workspace')==='other'&&document.querySelector('.page-heading strong')?.textContent==='other / app'&&document.querySelectorAll('.mail-row[data-state=pending]').length===2`), 'Mailbox selector must include workspace and project');
      assert(await cdp.evaluate(`Array.from(document.querySelectorAll('.mail-preview small')).every(e=>e.textContent.includes('Gemini CLI'))`));
      await cdp.evaluate('document.getElementById("compose-shortcut").click()');
      assert.equal(new URL(await cdp.evaluate('location.href')).searchParams.get('workspace'), 'other');
      await cdp.evaluate(`document.querySelector('[data-mailbox-workspace="team"][data-mailbox-project="app"]').click()`);
      await until(() => cdp.evaluate(`document.querySelector('.page-heading strong')?.textContent==='team / app'&&document.querySelectorAll('.mail-row[data-state=pending]').length===1`), 'Queue link must open the named project');
      for (const width of [320,390,768]) {
        await cdp.send('Emulation.setDeviceMetricsOverride', {width,height:960,deviceScaleFactor:1,mobile:width<640});
        assert(await cdp.evaluate(`document.documentElement.scrollWidth<=innerWidth&&Array.from(document.querySelectorAll('.mailbox-link')).every(a=>{const r=a.getBoundingClientRect();return r.left>=0&&r.right<=innerWidth&&r.height>=44;})`), 'Mailbox links overflow or are too small');
      }
      await cdp.goto(base + '/web/inbox', '#mail-project');
      assert.equal(await cdp.evaluate('document.getElementById("mail-project").selectedOptions[0].dataset.project'), 'app', 'Default should prefer a project with queued mail');
      const claimed = spawnSync(binary, ['message','pop','--workspace','other','--project','app','--agent','gemini-cli','--id',ids[1],'--json'], {env:{...env,AI_MEMORY_SERVER_URL:base,AI_MEMORY_DATA_DIR:path.join(artifacts,'client')},encoding:'utf8',timeout:15000});
      assert.equal(claimed.status, 0);assert.equal(JSON.parse(claimed.stdout).message.id, ids[1], 'Reading must leave the queued message available to its agent');
      await cdp.reload();
      assert(await cdp.evaluate(`document.querySelector('.mail-overview').textContent.includes('2 queued')&&document.querySelector('[data-mailbox-workspace="other"]').textContent.includes('1 queued')`), 'Collected messages must leave the queued counts');
    },
    async responsive() {
      await cdp.goto(project, '#project-panel-overview');
      for (const width of [320,390,768]) {
        await cdp.send('Emulation.setDeviceMetricsOverride', {width, height: 960, deviceScaleFactor: 1, mobile: width < 640});
        for (const name of ['overview','agents','tokens','sessions','knowledge']) {
          await tab(name);
          assert(await cdp.evaluate(`document.documentElement.scrollWidth<=innerWidth&&Array.from(document.querySelectorAll('.project-tabs a,.project-tools a')).every(a=>{const r=a.getBoundingClientRect();return r.left>=0&&r.right<=innerWidth&&r.height>=44;})`), `Navigation overflow or small target at ${width}px`);
        }
      }
    },
  };
  assert(!selectedCase || Object.hasOwn(cases, selectedCase), 'Unknown browser case');
  assert(!selectedCase || !mutation || mutations[mutation].test === selectedCase, 'Mutation targets another browser case');
  for (const [name, run] of Object.entries(cases)) {
    if (selectedCase && selectedCase !== name) continue;
    if (mutation && mutations[mutation].test !== name) continue;
    try {await run();report.cases.push({name, passed: true});console.log('PASS ' + name);}
    catch (error) {
      report.cases.push({name, passed: false, error: error.message});console.error('FAIL ' + name + ': ' + error.message);
      const shot = await cdp.send('Page.captureScreenshot', {format: 'png'}).catch(() => null);
      if (shot) fs.writeFileSync(path.join(artifacts, name + '.png'), Buffer.from(shot.data, 'base64'));
    }
  }
  assert.deepEqual(cdp.errors, [], 'Browser JavaScript errors');
  assert(!mutation || report.mutationApplied > 0, 'Mutation was not applied');
  assert(report.cases.every(c => c.passed), 'Browser regression checks failed');
}

try {await main();}
catch (error) {report.error = error.message;process.exitCode = 1;console.error(error.message);}
finally {
  if (cdp) cdp.socket.close();
  for (const child of children.reverse()) {
    if (child.exitCode === null) child.kill('SIGTERM');
    let timer;
    const result = await Promise.race([child.completion, new Promise(resolve => {timer=setTimeout(() => resolve(null),5000);})]);
    clearTimeout(timer);
    if (!result && child.exitCode === null) {child.kill('SIGKILL');await child.completion;}
  }
  fs.writeFileSync(path.join(artifacts, 'report.json'), JSON.stringify(report, null, 2));
  console.log('Artifacts: ' + artifacts);
}
