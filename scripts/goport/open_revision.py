#!/usr/bin/env python3
"""Records a new bound revision in the saved typechecker state before any measurement.

With --new-batch it first saves the current (accepted) batch record under docs/typechecker-batches/,
adds it to batchRecords and opens the new batch with protectedSet "goport": goport's own tests and
the gate items are the protected set (docs/typechecker-accountability.md, "Protected set"). That needs
Theo's standing rule goport-protected-set (batchId "*") in acceptanceRuleChanges. Nothing is extended
per batch. The new batch records protectedBase: the accepted batch whose goport test results and gate
manifest (and LSP and API oracle results, with its oracle answer sets) the candidate is compared with
(see protected_base).
Without --new-batch it adds the revision to the current goport batch (refused when that batch is
already accepted).

Before any write it checks the batch checkout: fp.py must equal --fingerprint, --commit must be HEAD,
and every .rs file changed since the previous revision's commit must be rustfmt clean
(rustfmt --edition 2024).

Usage:
  scripts/goport/open_revision.py --revision 132 --fingerprint <sha256> --commit <sha>
      --hypothesis "<text>" --change "<text>" [--new-batch <batch-id> --origin "<text>"] [--dry-run]
  scripts/goport/open_revision.py --base      prints protected_base of the saved state as JSON
  scripts/goport/open_revision.py --allowed   prints the allowedChangedFiles of a new goport batch
  scripts/goport/open_revision.py --protected prints the protected paths (globs) of PROTECTED
Writes through scripts/state (export, then import). --dry-run runs the checks and prints the new
history row, but writes nothing. Run from the repository root.
"""
import argparse, copy, datetime, hashlib, json, os, re, subprocess, sys, tempfile

GOPORT_RULE = 'goport-protected-set'
KEEP = ['checkout', 'writerOutputDirectory', 'phase', 'implementer', 'carryForward', 'openDefects']
HERE = os.path.dirname(os.path.abspath(__file__))  # scripts/goport of this checkout
REPO = os.path.dirname(os.path.dirname(HERE))


def runner_scripts():
    """The repository files that the gate and the bound runs run or read, read from gate.sh and bound2.sh
    themselves: each $HERE/<name> (a file next to gate.sh) and each scripts/... path in their text that
    exists. A new gate stage script is protected with no edit here. Their stages under target/ are
    outside every batch scope (target/**)."""
    found = set()
    for runner in ('gate.sh', 'bound2.sh'):
        text = open(os.path.join(HERE, runner)).read()
        found.update(f'scripts/goport/{m}' for m in re.findall(r'\$HERE/([\w.-]+)', text))
        found.update(re.findall(r'(?<![\w.-])(scripts/[\w./-]+\.(?:sh|py|mjs|txt))', text))
    return sorted(p for p in found if os.path.isfile(os.path.join(REPO, p)))


# Paths that judge the protected set: the check and state tools, the pipeline, the runners, the
# compare tools, the gate, its stage scripts and its allow list, remote.sh (it copies the scripts and
# bins that the gate and the oracles run on a host), the oracles, the baseline and the rules. Also
# UPSTREAM.json (for each pin, the oracle binary and sha256 and the Go checkout that pin.py binds for
# the gate, the oracles and the Go baselines of goport-tests.sh) and run-cargo-capped.sh (it builds the
# test and release bins and runs clippy for the quality record). A candidate
# that edits one would be judged by the edited copy after its merge, so a loss could pass in two steps.
# candidate.sh check fails when the candidate branch changes one (git diff from its merge base with
# main), unless the batch's allowedChangedFiles lists that exact path (a batch that must change a
# tool, with Theo's approval).
PROTECTED = list(dict.fromkeys([
    'AGENTS.md', 'docs/typechecker-accountability.md', 'docs/goport-protected/**',
    'scripts/check-typechecker-batch.mjs', 'scripts/state.mjs', 'scripts/state', 'scripts/upstream/pin.py',
    'UPSTREAM.json', 'scripts/run-cargo-capped.sh',
    *(f'scripts/goport/{f}' for f in ('candidate.sh', 'open_revision.py', 'accept_revision.py', 'fp.py',
                                      'build-goport-tests.sh', 'goport-tests.sh', 'compare-tests.py',
                                      'gate.sh', 'gate-allow.txt', 'gate-compare.py', 'ls_edit_bench.py',
                                      'bound2.sh', 'lsp_oracle.py', 'api_oracle.py', 'oracle-compare.py',
                                      'np-suite.sh', 'remote.sh', 'purge-foreign-fingerprints.py')),
    *runner_scripts()]))
# Scope of a goport batch: the whole repository except the saved state, target/ and PROTECTED.
# candidate.sh check and open read it through --allowed. "!" entries exclude.
ALLOWED = ['**', '!docs/typechecker-state/**', '!docs/typechecker-batches/**', '!target/**', *(f'!{p}' for p in PROTECTED)]
# Evidence of one revision. open clears it, so a new revision never shows the last one's evidence.
EVIDENCE = ['goportTests', 'gateCompare', 'gate', 'gateRuns', 'gateVerdict', 'languageServerOracle', 'apiOracle', 'quality',
            'qualityEvidence', 'ordinaryQuery', 'localCheck', 'acceptance']
# A new batch gets a new auditor and reviewer (AGENTS.md). Root records their agent ids with
# scripts/state record current before the verdicts; accept_revision.py refuses a pending id.
AUDITOR = {'role': 'audit_accepted_roster', 'agent': 'pending-new-auditor', 'verdict': 'PENDING'}
REVIEWER = {'role': 'independent_reviewer', 'agent': 'pending-new-reviewer', 'verdict': 'PENDING'}


def sha(path):
    return hashlib.sha256(open(path, 'rb').read()).hexdigest()


def git(checkout, *args):
    return subprocess.check_output(['git', '-C', checkout, *args])


def export():
    return json.loads(subprocess.check_output(['node', 'scripts/state.mjs', 'export']))


def goport_rule(state):
    rule = next((r for r in state.get('acceptanceRuleChanges', []) if r.get('id') == GOPORT_RULE and r.get('batchId') == '*'), None)
    if not rule or rule.get('protectedSet') != 'goport' or not (rule.get('baseline') or {}).get('path'):
        sys.exit(f'no standing {GOPORT_RULE} rule (batchId "*", protectedSet "goport", baseline) in acceptanceRuleChanges; '
                 'root records it first (legacy removal stage 1)')
    return rule


def oracle_base(record):
    """{label, dir} of the LSP or API oracle results that a batch field (languageServerOracle, apiOracle)
    or the rule's apiBaseline names. An older record has only the summary path results/<label>/summary.md."""
    if not record:
        return None
    d = record.get('dir') or (os.path.dirname(record['summary']) if record.get('summary') else None)
    return {'label': record.get('label') or os.path.basename(d), 'dir': d} if d else None


def protected_base(state):
    """The base of the next candidate: the last accepted batch, with {path, sha256} of its goport test
    results (the rule's baseline when that batch used the legacy roster) and of its gate manifest,
    {label, dir} of its LSP and API oracle results (the rule's apiBaseline for a legacy batch, which
    had no API run), and its oracleAnswers (the answer sets that keep the flake requests of a pin bump
    protected; oracle-compare.py --answers). An open goport batch keeps the base that open recorded."""
    b = state['batch']
    if b.get('compilerAccepted') is not True:
        if not b.get('protectedBase'):
            sys.exit(f'batch {b["id"]} is not accepted and has no protectedBase')
        return b['protectedBase']
    goport = b.get('protectedSet') == 'goport'
    tests = ({'path': b['goportTests']['results'], 'sha256': b['goportTests']['sha256']} if goport
             else {k: goport_rule(state)['baseline'][k] for k in ('path', 'sha256')})
    return {'batch': b['id'], 'revision': b['recoveryRevision'], 'tests': tests,
            'gate': {'path': b['gate']['manifest'], 'sha256': b['gate']['sha256']},
            'lsp': oracle_base(b.get('languageServerOracle')),
            'api': oracle_base(b.get('apiOracle') if goport else goport_rule(state).get('apiBaseline')),
            'oracleAnswers': b.get('oracleAnswers') or []}


def unformatted(checkout, base):
    """.rs files changed or added since commit base (working tree, untracked included) that
    rustfmt --edition 2024 would change. Each file is formatted alone from stdin, so the
    result does not depend on unchanged child modules."""
    names = git(checkout, 'diff', '--name-only', '-z', '--diff-filter=d', base, '--', '*.rs').split(b'\0')
    names += git(checkout, 'ls-files', '--others', '--exclude-standard', '-z', '--', '*.rs').split(b'\0')
    bad = []
    for name in sorted({n.decode() for n in names if n}):
        data = open(os.path.join(checkout, name), 'rb').read()
        run = subprocess.run(['rustfmt', '--edition', '2024', '--emit', 'stdout'], input=data, capture_output=True)
        if run.returncode != 0 or run.stdout != data:
            bad.append(name)
    return bad


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--base', action='store_true', help='print the protected base of the saved state and exit')
    p.add_argument('--allowed', action='store_true', help='print the allowedChangedFiles of a new goport batch and exit')
    p.add_argument('--protected', action='store_true', help='print the protected paths (globs) and exit')
    p.add_argument('--revision', type=int)
    p.add_argument('--fingerprint')
    p.add_argument('--commit')
    p.add_argument('--hypothesis')
    p.add_argument('--change')
    p.add_argument('--new-batch')
    p.add_argument('--origin', default='')
    p.add_argument('--dry-run', action='store_true')
    a = p.parse_args()
    if a.allowed or a.protected:
        print('\n'.join(ALLOWED if a.allowed else PROTECTED))
        return
    s = export()
    if a.base:
        print(json.dumps(protected_base(s)))
        return
    if None in (a.revision, a.fingerprint, a.commit, a.hypothesis, a.change):
        p.error('--revision, --fingerprint, --commit, --hypothesis and --change are required')
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    old = s['batch']
    last = old['recoveryHistory'][-1]
    if a.revision != last['revision'] + 1:
        sys.exit(f'revision {a.revision} is not the next revision ({last["revision"] + 1})')
    if not a.new_batch and old.get('compilerAccepted') is True:
        sys.exit(f'batch {old["id"]} is accepted; open R{a.revision} with --new-batch')
    if not a.new_batch and old.get('protectedSet') != 'goport':
        sys.exit(f'batch {old["id"]} uses the retired legacy roster; open R{a.revision} with --new-batch')

    checkout = old['checkout']
    head = git(checkout, 'rev-parse', 'HEAD').decode().strip()
    if git(checkout, 'rev-parse', '--verify', f'{a.commit}^{{commit}}').decode().strip() != head:
        sys.exit(f'--commit {a.commit} is not HEAD of {checkout} ({head[:12]})')
    fp = subprocess.check_output(['python3', 'scripts/goport/fp.py', checkout], text=True).split()[0]
    if fp != a.fingerprint:
        sys.exit(f'--fingerprint {a.fingerprint[:12]} does not match fp.py of {checkout} ({fp[:12]})')
    base = last.get('commit') or old.get('commit')
    if not base:
        sys.exit(f'R{last["revision"]} has no commit; cannot find the .rs files changed since it')
    bad = unformatted(checkout, base)
    if bad:
        sys.exit(f'rustfmt --edition 2024 would change {len(bad)} file(s) changed since {base}:\n  ' + '\n  '.join(bad))
    previous_fp = old['sourceFingerprint']

    record = None
    if a.new_batch:
        if old.get('compilerAccepted') is not True:
            sys.exit('the current batch is not accepted; finish it before opening a new one')
        goport_rule(s)
        protected = protected_base(s)
        for ref in (protected['tests'], protected['gate']):
            if sha(ref['path']) != ref['sha256']:
                sys.exit(f'protected base {ref["path"]} does not match its sha256 {ref["sha256"][:12]}')
        record = f"docs/typechecker-batches/{old['id']}.json"
        if os.path.exists(record):
            sys.exit(f'{record} exists')
        text = subprocess.check_output(['node', 'scripts/state.mjs', 'batch', '--with-history'])
        s['batchRecords'].append({'path': record, 'sha256': hashlib.sha256(text).hexdigest(), 'bytes': len(text)})
        b = {k: copy.deepcopy(old[k]) for k in KEEP if k in old}
        b.update({'id': a.new_batch, 'previousBatch': {'id': old['id'], 'archive': s['batchRecords'][-1]},
                  'latestHono': old.get('latestHono'), 'origin': a.origin, 'protectedSet': 'goport', 'protectedBase': protected,
                  'allowedChangedFiles': ALLOWED,
                  'compilerEditsAuthorized': True, 'productionEditsAuthorized': True, 'semanticEditsAuthorized': True,
                  'testEditsAuthorized': False, 'runtimeAuthorized': True, 'expectedCompilerRecoveries': [],
                  'expectationUpdates': [], 'verdictHistory': [], 'focusedResults': [], 'runtimeToolHandles': [],
                  'commands': [], 'interruptedRuns': [], 'recoveryHistory': old['recoveryHistory']})
        # A new batch stays at the Go pin its predecessor was accepted at (candidate.sh side reads
        # batch.upstreamPin.to). A pin-bump batch sets a new "to" itself.
        if old.get('upstreamPin'):
            pin = old['upstreamPin']['to']
            b['upstreamPin'] = {'from': pin, 'to': pin, 'rule': None, 'note': f"same pin as {old['id']}"}
        s['reason'] = f"Batch {old['id']} accepted at R{last['revision']}. Batch {a.new_batch} opened at R{a.revision}."
    else:
        b = old
        s['reason'] = f'R{a.revision} bound in batch {b["id"]}.'
    b.update({k: None for k in EVIDENCE})
    # A later revision in the same batch keeps the batch's auditor and reviewer; only the verdict resets.
    keep = lambda role, key: {**role, 'agent': (b.get(key) or {}).get('agent') or role['agent']} if not a.new_batch else dict(role)
    b.update({'recoveryRevision': a.revision, 'hypothesis': a.hypothesis, 'sourceFingerprint': a.fingerprint,
              'beforeEditingSourceFingerprint': previous_fp, 'sourceBindingStatus': 'SOURCE_BOUND',
              'commit': a.commit, 'completedRuns': [], 'compilerAccepted': False, 'passingCredit': False,
              'auditor': keep(AUDITOR, 'auditor'), 'reviewer': keep(REVIEWER, 'reviewer'),
              'nextPermittedAction': f'Run the R{a.revision} pipeline (candidate.sh side).'})
    row = {'revision': a.revision, 'hypothesis': a.hypothesis, 'hypothesisLabel': b['id'].replace('recovery-continuation-', ''),
           'phase': 'recovery-continuation', 'sourceFingerprint': a.fingerprint, 'beforeEditingSourceFingerprint': previous_fp,
           'protectedSet': 'goport', 'fullResultSha256': None, 'status': 'bound_before_full_measurement',
           'recordedUtc': now, 'commit': a.commit, 'change': a.change}
    b['recoveryHistory'] = b['recoveryHistory'] + [row]
    s['batch'] = b
    s['status'], s['decision'], s['updated'] = 'active', 'REVIEW', now
    if a.dry_run:
        print(json.dumps({'dryRun': True, 'batch': b['id'], 'batchRecord': record, 'protectedBase': b['protectedBase'], 'row': row}, indent=1))
        return
    if record:
        with open(record, 'xb') as f:
            f.write(text)
    # A temp file of its own, so two runs at the same time do not write one file.
    with tempfile.NamedTemporaryFile('w', prefix=f'open-revision-{a.revision}-', suffix='.json') as f:
        json.dump(s, f)
        f.flush()
        subprocess.check_call(['node', 'scripts/state.mjs', 'import', f.name])


if __name__ == '__main__':
    main()
