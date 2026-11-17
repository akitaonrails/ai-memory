#!/usr/bin/env node
// A passing baseline is required before a deliberate behavioral fault can count.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const manifest = {baseline: null, mutants: []};
const faults = [
  ['all-panels-visible', 'tabs'],
  ['ignore-recipient-workspace', 'recipients'],
  ['ignore-recipient-project', 'recipients'],
  ['retain-write-token', 'delivery'],
  ['disable-live-refresh', 'live-usage'],
  ['disable-session-live-refresh', 'session-live-usage'],
  ['drop-usage-query', 'call-history'],
  ['forget-theme', 'themes'],
  ['resolve-fragments-at-mount', 'fragment-navigation'],
  ['steal-browser-shortcut', 'tabs'],
  ['enable-send-while-pending', 'delivery'],
  ['enable-send-for-empty-project-after-request', 'delivery'],
  ['disable-release-auto-submit', 'documentation'],
  ['restore-stale-release-on-history', 'documentation'],
  ['show-false-inbox-alert', 'mailbox-context'],
  ['ignore-mailbox-workspace', 'mailbox-context'],
  ['disable-inbox-notifications', 'notifications'],
  ['ignore-notification-workspace', 'notifications'],
  ['skip-message-edit-hold', 'message-editing'],
  ['lose-saved-message-text', 'message-editing'],
  ['save-discarded-message-text', 'message-editing'],
];

const selectedCase = process.argv.find(arg => arg.startsWith('--case='))?.split('=')[1];
assert(!selectedCase || faults.some(([, name]) => name === selectedCase), 'Unknown browser mutation case');

async function run(mutation) {
  const child = spawn(process.execPath, [path.join(here, 'web_dashboard_browser.mjs'), ...(mutation ? ['--mutation=' + mutation] : []), ...(selectedCase ? ['--case=' + selectedCase] : [])], {env: process.env});
  let output = '', errors = '';
  child.stdout.on('data', data => {output += data;});
  child.stderr.on('data', data => {errors += data;});
  const timer = setTimeout(() => child.kill('SIGKILL'), 90000);
  const code = await new Promise((resolve, reject) => {child.on('error', reject);child.on('exit', resolve);});
  clearTimeout(timer);
  const directory = output.match(/^Artifacts: (.+)$/m)?.[1];
  assert(directory, 'Browser runner did not produce a report: ' + errors.slice(-500));
  return {
    code,
    directory,
    errors: errors.trim().slice(-2000),
    result: JSON.parse(fs.readFileSync(path.join(directory, 'report.json'), 'utf8')),
  };
}

function describe(run) {
  return JSON.stringify({
    code: run.code,
    directory: run.directory,
    error: run.result.error,
    failedCases: run.result.cases?.filter(test => !test.passed) || [],
    stderr: run.errors,
  });
}

function failedBeforeMutation(run) {
  return run.result.mutationApplied === 0
    && run.result.error
    && run.result.error !== 'Mutation was not applied';
}

try {
  const baseline = await run();
  const failedCases = baseline.result.cases?.filter(test => !test.passed) || [];
  assert.equal(
    baseline.code,
    0,
    'Baseline must pass; inspect ' + baseline.directory + ': ' + JSON.stringify({
      error: baseline.result.error,
      failedCases,
      stderr: baseline.errors,
    }),
  );
  assert((selectedCase ? baseline.result.cases.length === 1 : baseline.result.cases.length >= 17) && baseline.result.cases.every(c => c.passed));
  manifest.baseline = baseline.directory;
  console.log('PASS unmodified server and browser baseline');
  for (const [fault, name] of faults) {
    if (selectedCase && selectedCase !== name) continue;
    let mutant = await run(fault);
    if (failedBeforeMutation(mutant)) {
      console.warn('RETRY ' + fault + ' after pre-application failure: ' + describe(mutant));
      mutant = await run(fault);
    }
    assert.equal(mutant.code, 1, fault + ' survived or the runner crashed: ' + describe(mutant));
    assert(mutant.result.mutationApplied > 0, fault + ' never reached the served application: ' + describe(mutant));
    assert.equal(mutant.result.error, 'Browser regression checks failed', 'An unrelated failure must not count as a killed mutant: ' + describe(mutant));
    assert(mutant.result.cases.some(c => c.name === name && c.passed === false), fault + ' escaped its behavioral test');
    manifest.mutants.push({fault, caughtBy: name, killed: true, artifacts: mutant.directory});
    console.log('KILLED ' + fault + ' by ' + name);
  }
} catch (error) {manifest.error = error.message;console.error(error.message);process.exitCode = 1;}
finally {
  const output = path.resolve(here, '../../target/web-dashboard-mutations.json');
  fs.mkdirSync(path.dirname(output), {recursive: true});
  fs.writeFileSync(output, JSON.stringify(manifest, null, 2));
  console.log('Mutation report: ' + output);
}
