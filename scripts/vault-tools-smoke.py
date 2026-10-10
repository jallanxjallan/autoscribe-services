from pathlib import Path
import subprocess, tempfile, os, json, hashlib, sys

BIN=Path(sys.argv[1])
ADAPTER=Path(sys.argv[2])
with tempfile.TemporaryDirectory(prefix='vault-management-') as tmp:
    home=Path(tmp); master=home/'.config/obsidian-vault';vault=home/'Test Vault';repos=home/'Dropbox/Repos'
    for p in [master/'config',master/'plugins/example',master/'tools/templates',vault/'.obsidian',repos]:p.mkdir(parents=True,exist_ok=True)
    (master/'config/hotkeys.json').write_text('{}')
    (master/'config/community-plugins.json').write_text('["example"]')
    (master/'plugins/example/manifest.json').write_text('{"id":"example"}')
    (master/'plugins/example/main.js').write_text('// fixture')
    (master/'tools/templates/article.md').write_text('---\nslug: fixture\n---\nTemplate\n')
    note=vault/'note.md'; note.write_bytes(b'---\r\nslug: keep-this\r\n# raw\r\n---\r\nBody\r\n');note_bytes=note.read_bytes()
    env=os.environ.copy()
    for name in ['OBSIDIAN_VAULT_SOURCE','OBSIDIAN_MASTER_VAULT','VAULT_REPOS_ROOT']:env.pop(name,None)
    env.update(HOME=str(home),VAULT_GIT_ADAPTER=str(ADAPTER),GIT_AUTHOR_NAME='Vault Tools Test',GIT_COMMITTER_NAME='Vault Tools Test',GIT_AUTHOR_EMAIL='test@example.invalid',GIT_COMMITTER_EMAIL='test@example.invalid')
    def run(name,*args,ok=True):
        r=subprocess.run([str(BIN/name),*args],cwd=vault,env=env,text=True,capture_output=True)
        assert (r.returncode==0)==ok,(name,args,r.stdout,r.stderr)
        return r
    def git(root,*args):
        return subprocess.run(['/usr/bin/python3',str(ADAPTER),'-C',str(root),*args],env=env,text=True,capture_output=True,check=True).stdout.strip()
    def snapshot(root):return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest() for p in root.rglob('*') if p.is_file() and '.git' not in p.parts}
    run('init-project-vault')
    assert (repos/'test_vault.git').is_dir()
    assert git(repos/'test_vault.git','symbolic-ref','--short','HEAD')=='main'
    assert git(vault,'rev-parse','HEAD')==git(repos/'test_vault.git','rev-parse','HEAD')
    run('init-project-vault')
    assert 'already current' in run('update-project-vault').stdout
    assert 'already current' in run('update-master-vault','apply=False').stdout
    config=home/'options.json';config.write_text(json.dumps({'master':'does-not-exist','repos_root':'Dropbox/Repos'}))
    run('update-project-vault',f'config={config}',f'master="{master}"','prune=False')
    run('update-project-vault','--config',str(config),'--master',str(master))
    before=snapshot(vault)
    run('update-project-vault',f'config={home/"missing.json"}',ok=False)
    config.write_text('{"apply":true}')
    run('update-project-vault',f'config={config}',ok=False)
    run('update-project-vault','unknown=value',ok=False)
    assert snapshot(vault)==before
    (vault/'.obsidian/hotkeys.json').write_text('{"fixture":[]}')
    before=snapshot(master)
    run('update-master-vault')
    assert snapshot(master)==before
    # Explicit writes still require a clean, tracked master.
    git(master,'init','-b','main');git(master,'add','--all');git(master,'commit','-m','Fixture master')
    run('update-master-vault','apply=True','allow_sensitive=False')
    assert (master/'config/hotkeys.json').read_text()=='{"fixture":[]}'
    assert note.read_bytes()==note_bytes
print('PASS: default init + bare backup, idempotent update, dry-run/apply safeguards, keyword/config precedence, invalid inputs, and raw note preservation')
