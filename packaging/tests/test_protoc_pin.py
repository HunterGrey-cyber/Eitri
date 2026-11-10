"""Socket-free bundled compiler checks; npm and downloads are local stand-ins."""
import hashlib
import io
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile

import pytest

PACKAGING = Path(__file__).resolve().parents[1]


@pytest.fixture
def tmp_path():
    scratch = Path.home() / '.cache/eitri-protoc-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        yield Path(directory)


def installer(path=PACKAGING / 'install.sh'):
    return path.read_text().rsplit('main "$@" </dev/null', 1)[0] + '\n}\n'


def run(script, cwd, shell='sh'):
    path = cwd / 'test-script.sh'
    path.write_text(script)
    return subprocess.run([shell, str(path)], cwd=cwd, text=True, capture_output=True,
                          env=dict(os.environ, HOME=str(cwd)), timeout=30)


def release(version='0.3.1', pin=''):
    return f'''EITRI_VERSION={version}
EITRI_COMMIT={'a' * 40}
NEOVIDE_FORK_COMMIT={'b' * 40}
VERDANDI_REV={'c' * 40}
VERDANDI_SOURCE=verdandi-ccccccc-source.tar.gz
VERDANDI_SOURCE_SHA256={'d' * 64}
NODE_VERSION=v22.23.2
NODE_SHA256_linux_x64={'e' * 64}
NODE_SHA256_linux_arm64={'f' * 64}
NVIM_VERSION=0.12.6
NVIM_SHA256_linux_x86_64={'a' * 64}
{pin}'''


def pin(sha):
    return f'GRPC_TOOLS_VERSION=1.13.1\nGRPC_TOOLS_SHA256_linux_x64={sha}\nGRPC_TOOLS_SHA256_linux_arm64={sha}\n'


def archive(path, files):
    with tarfile.open(path, 'w:gz') as tar:
        for name, contents in files.items():
            info = tarfile.TarInfo(name)
            if contents is None:
                info.type = tarfile.DIRTYPE
            else:
                contents = contents.encode()
                info.size = len(contents)
            info.mode = 0o755
            tar.addfile(info, None if contents is None else io.BytesIO(contents))


@pytest.fixture
def build(tmp_path):
    archive(tmp_path / 'compiler.tgz', {'bin/': None, 'bin/protoc': '#!/bin/sh\nexit 0\n',
                                       'bin/grpc_node_plugin': '#!/bin/sh\nexit 0\n'})
    sha = hashlib.sha256((tmp_path / 'compiler.tgz').read_bytes()).hexdigest()
    archive(tmp_path / 'node.tgz', {
        'node-v22.23.2-linux-x64/bin/node': '#!/bin/sh\nexit 0\n',
        'node-v22.23.2-linux-x64/bin/npm': '''#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$HOME/npm.log"
if [ "$1" = ci ]; then
    mkdir -p node_modules/grpc-tools
    printf '{"version":"1.13.1"}\n' > node_modules/grpc-tools/package.json
else
    test -x node_modules/grpc-tools/bin/protoc
    mkdir -p apps/claude-sidecar/dist-bin
    printf '#!/bin/sh\necho "fixture (protocol 3, node v22.23.2, host_cli)"\n' > apps/claude-sidecar/dist-bin/verdandi-claude-sidecar-test-linux-x64
    chmod +x apps/claude-sidecar/dist-bin/*
fi
''',
    })
    archive(tmp_path / 'source.tgz', {'package.json': '{}', 'apps/claude-sidecar/package.json': '{}'})
    prelude = installer() + '''
NV_NODE_ARCH=x64
curl_get() { cp "$HOME/compiler.tgz" "$2"; CG_OK=1; }
sidecar_sdk_notice() { :; }
accept_sidecar_artifact() { AA_VERSION_LINE=fixture; }
'''
    return tmp_path, sha, prelude


@pytest.mark.parametrize('route', ['install', 'setup', 'aur'])
def test_bundled_build_ignores_scripts_and_places_verified_protoc(build, route):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(pin=pin(sha)))
    # Both per-user entry points call install_sidecar_for_rev; AUR shares the build core.
    script += '''
parse_release "$HOME/RELEASE"
NV_CACHE_NV=$HOME/cache
NV_SIDECAR_ROOT=$HOME/sidecar
mkdir -p "$NV_CACHE_NV"
check_sidecar_build_space() { :; }
sidecar_download_node() { SDN_OK=1; SDN_TARBALL=$HOME/node.tgz; }
sidecar_download_verdandi_source() { SDV_TARBALL=$HOME/source.tgz; }
'''
    if route == 'aur':
        script += '''
resolve_setup_release() { :; }
acquire_lock() { :; }
OPT_NODE_TARBALL=$HOME/node.tgz
OPT_VERDANDI_SOURCE_TARBALL=$HOME/source.tgz
OPT_BUILD_SIDECAR_INTO=$HOME/out
REL_NODE_SHA256_LINUX_X64=$(file_sha256 "$OPT_NODE_TARBALL")
REL_VERDANDI_SOURCE_SHA256=$(file_sha256 "$OPT_VERDANDI_SOURCE_TARBALL")
do_build_sidecar_into
'''
    else:
        script += 'install_sidecar_for_rev ccccccc ' + ('1' if route == 'install' else '0') + '\n'
    proc = run(script, root)
    assert proc.returncode == 0, proc.stderr
    assert 'ci --no-audit --no-fund --ignore-scripts' in (root / 'npm.log').read_text()


@pytest.mark.parametrize('arch', ['x64', 'arm64'])
def test_bundled_sha_mismatch_refuses_before_npm(build, arch):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(pin=pin('0' * 64)))
    proc = run(script + f'''
parse_release "$HOME/RELEASE"
NV_NODE_ARCH={arch}
mkdir "$HOME/work"
sidecar_build_core "$HOME/work" "$HOME/node.tgz" "$HOME/source.tgz"
''', root)
    assert proc.returncode != 0
    assert 'unverified protoc' in proc.stderr
    assert not (root / 'npm.log').exists()


@pytest.mark.parametrize('version,pinned,allowed', [('0.3.0', False, True), ('0.2.2', False, True),
                                                   ('0.3.1', False, False), ('0.3.1', True, True)])
def test_new_installer_old_release_compatibility(build, version, pinned, allowed):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(version, pin(sha) if pinned else ''))
    proc = run(script + '''
parse_release "$HOME/RELEASE"
mkdir "$HOME/work"
prepare_bundled_protoc "$HOME/work"
printf 'flags=%s archive=%s\n' "$SB_NPM_CI_FLAGS" "$SB_PROTOC_TARBALL"
''', root)
    assert (proc.returncode == 0) == allowed, proc.stderr
    if pinned:
        assert 'flags=--ignore-scripts' in proc.stdout
        assert 'warning:' not in proc.stderr
        return
    assert 'upgrade to a release' in proc.stderr
    if allowed:
        assert 'warning:' in proc.stderr
        assert 'flags= archive=' in proc.stdout


@pytest.mark.parametrize('bad', ['GRPC_TOOLS_VERSION=bad\n', 'GRPC_TOOLS_VERSION=\n', pin('x' * 64)])
def test_partial_or_malformed_pin_never_falls_back(build, bad):
    root, sha, script = build
    (root / 'RELEASE').write_text(release('0.3.0', bad))
    proc = run(script + 'parse_release "$HOME/RELEASE"\nprepare_bundled_protoc "$HOME"\n', root)
    assert proc.returncode != 0
    assert 'GRPC_TOOLS' in proc.stderr
    assert 'warning:' not in proc.stderr


def test_old_installer_ignores_new_fields(tmp_path):
    (tmp_path / 'RELEASE').write_text(release(pin=pin('a' * 64)))
    old = PACKAGING / 'tests/install/fixtures/install-0.2.0.sh'
    proc = run(installer(old) + 'parse_release "$HOME/RELEASE"\necho "$REL_REV7"\n', tmp_path)
    assert proc.returncode == 0, proc.stderr
    assert proc.stdout.strip() == 'ccccccc'


def test_release_writes_compiler_pin_and_signature_covers_it(tmp_path):
    # The offline writer copies the checked-in pin into the signed manifest.
    repo = tmp_path / 'source'
    repo.mkdir()
    (repo / 'package-lock.json').write_text('{"packages":{"node_modules/grpc-tools":{"version":"1.13.1"}}}')
    for args in (['init', '-q'], ['add', '.'], ['-c', 'user.name=Test', '-c', 'user.email=test@example.com',
                                              '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture']):
        subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
    (tmp_path / 'pins.env').write_text(pin('a' * 64))
    script = f'source {shlex.quote(str(PACKAGING / "release.sh"))}\n' + '''
RS_VERSION=0.3.1 RS_COMMIT=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
RS_FORK_COMMIT=$RS_COMMIT RS_VERDANDI_REV=$RS_COMMIT RS_VERDANDI_SOURCE_SHA256=x
RS_NODE_VERSION=v22.23.2 RS_NODE_SHA256_X64=x RS_NODE_SHA256_ARM64=x RS_NVIM_VERSION=
RS_SKIA_ARCHIVE=x RS_SKIA_SHA256=x RS_BUILD_IMAGE=x RS_REHEARSAL=0
read_pins "$HOME/pins.env"
write_release "$HOME/RELEASE"
ssh-keygen -q -t ed25519 -N '' -f "$HOME/key"
printf 'release@eitri namespaces="eitri-release" %s\n' "$(cat "$HOME/key.pub")" > "$HOME/signers"
write_sums "$HOME" RELEASE
sign_sums "$HOME/key" "$HOME"
verify_sums "$HOME/signers" "$HOME"
sha256sum -c "$HOME/SHA256SUMS"
'''
    proc = run(script, tmp_path, 'bash')
    assert proc.returncode == 0, proc.stderr
    text = (tmp_path / 'RELEASE').read_text()
    assert 'GRPC_TOOLS_VERSION=1.13.1\n' in text
    for arch in ('x64', 'arm64'):
        assert f'GRPC_TOOLS_SHA256_linux_{arch}={'a' * 64}\n' in text
    (tmp_path / 'RELEASE').write_text(text.replace('1.13.1', '1.13.2'))
    assert run('sha256sum -c SHA256SUMS', tmp_path).returncode != 0
    rewritten_sums = f'source {shlex.quote(str(PACKAGING / "release.sh"))}\n' + '''
write_sums "$HOME" RELEASE
verify_sums "$HOME/signers" "$HOME"
'''
    assert run(rewritten_sums, tmp_path, 'bash').returncode != 0


@pytest.mark.parametrize('arch', ['x64', 'arm64'])
def test_pin_selects_matching_architecture(build, arch):
    root, sha, script = build
    fields = pin('0' * 64).replace(f'GRPC_TOOLS_SHA256_linux_{arch}=' + '0' * 64,
                                 f'GRPC_TOOLS_SHA256_linux_{arch}={sha}')
    (root / 'RELEASE').write_text(release(pin=fields))
    proc = run(script + f'''
parse_release "$HOME/RELEASE"
NV_NODE_ARCH={arch}
curl_get() {{
    test "$1" = "https://node-precompiled-binaries.grpc.io/grpc-tools/v1.13.1/linux-{arch}.tar.gz" || exit 91
    cp "$HOME/compiler.tgz" "$2"; CG_OK=1
}}
prepare_bundled_protoc "$HOME"
''', root)
    assert proc.returncode == 0, proc.stderr
    assert (root / f'linux-{arch}.tar.gz').read_bytes() == (root / 'compiler.tgz').read_bytes()


@pytest.mark.parametrize('shape', ['valid', 'extra', 'missing', 'duplicate', 'corrupt'])
def test_release_downloads_both_architectures_for_the_locked_version(tmp_path, shape):
    repo = tmp_path / 'repo'
    repo.mkdir()
    (repo / 'package-lock.json').write_text('{"packages":{"node_modules/grpc-tools":{"version":"1.13.1"}}}')
    for args in (['init', '-q'], ['add', '.'], ['-c', 'user.name=Test', '-c', 'user.email=test@example.com',
                                              '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture']):
        subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
    members = {'bin/': None, 'bin/protoc': 'compiler', 'bin/grpc_node_plugin': 'plugin'}
    if shape == 'extra':
        members['bin/unexpected'] = 'extra'
    if shape == 'missing':
        del members['bin/protoc']
    if shape == 'duplicate':
        members['bin'] = None
    archive(tmp_path / 'compiler.tgz', members)
    if shape == 'corrupt':
        (tmp_path / 'compiler.tgz').write_text('not a tarball')
    digest = hashlib.sha256((tmp_path / 'compiler.tgz').read_bytes()).hexdigest()
    (tmp_path / 'pins.env').write_text(pin(digest))
    script = f'source {shlex.quote(str(PACKAGING / "release.sh"))}\n' + '''
curl() {
    printf '%s\n' "$*" >> "$HOME/downloads.log"
    while [ "$1" != --output ]; do shift; done
    cp "$HOME/compiler.tgz" "$2"
}
fetch_protoc_pin "$HOME/repo" HEAD "$HOME/downloads"
read_pins "$HOME/pins.env"
printf '%s\n' "$RS_GRPC_TOOLS_VERSION" "$RS_GRPC_TOOLS_SHA256_X64" "$RS_GRPC_TOOLS_SHA256_ARM64"
'''
    proc = run(script, tmp_path, 'bash')
    if shape != 'valid':
        assert proc.returncode != 0
        assert 'protoc' in proc.stderr
        return
    assert proc.returncode == 0, proc.stderr
    log = (tmp_path / 'downloads.log').read_text()
    expected = ['1.13.1']
    for arch in ('x64', 'arm64'):
        url = f'https://node-precompiled-binaries.grpc.io/grpc-tools/v1.13.1/linux-{arch}.tar.gz'
        assert url in log
        expected.append(digest)
    assert proc.stdout.splitlines() == expected


def test_bundled_build_refuses_a_different_installed_grpc_tools_version(build):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(pin=pin(sha).replace('1.13.1', '1.13.2')))
    proc = run(script + '''
parse_release "$HOME/RELEASE"
mkdir "$HOME/work"
sidecar_build_core "$HOME/work" "$HOME/node.tgz" "$HOME/source.tgz"
''', root)
    assert proc.returncode != 0
    assert 'not the 1.13.2 this release pins protoc for' in proc.stderr
    assert 'build:binary' not in (root / 'npm.log').read_text()


@pytest.mark.parametrize('mismatch', ['', 'version', 'x64', 'arm64', 'malformed', 'missing'])
def test_release_checks_locked_version_and_downloads_against_pins(tmp_path, mismatch):
    (tmp_path / 'package-lock.json').write_text('{"packages":{"node_modules/grpc-tools":{"version":"1.13.1"}}}')
    for arch in ('x64', 'arm64'):
        (tmp_path / f'linux-{arch}.tar.gz').write_text('archive')
    digest = hashlib.sha256(b'archive').hexdigest()
    fields = pin(digest)
    if mismatch == 'version':
        fields = fields.replace('1.13.1', '1.13.2')
    elif mismatch in ('x64', 'arm64'):
        (tmp_path / f'linux-{mismatch}.tar.gz').write_text('changed')
    elif mismatch == 'malformed':
        fields = fields.replace(digest, 'invalid')
    elif mismatch == 'missing':
        fields = ''
    (tmp_path / 'pins.env').write_text(fields)
    proc = subprocess.run(['python3', str(PACKAGING / 'release_check.py'), 'check-protoc-pin',
                           str(tmp_path / 'package-lock.json'), str(tmp_path / 'pins.env'), str(tmp_path)],
                          capture_output=True, text=True, timeout=30)
    assert (proc.returncode == 0) == (not mismatch), proc.stderr
    if mismatch:
        assert 'pins.env' in proc.stderr


@pytest.mark.parametrize('version,fields,allowed', [
    ('0.3.0', '', True), ('0.3.1', '', False), ('0.3.1', pin('a' * 64), True),
    ('0.3.0', 'GRPC_TOOLS_VERSION=\n', False), ('0.3.1', pin('bad'), False),
    ('0.3.1', pin('a' * 64).replace('1.13.1', 'v1.13.1'), False),
])
@pytest.mark.parametrize('route', ['install', 'setup', 'aur'])
def test_dry_run_checks_compiler_metadata_without_download(build, version, fields, allowed, route):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(version, fields))
    script += '''
parse_release "$HOME/RELEASE"
OPT_RELEASE_FILE=$HOME/RELEASE
NV_NEW_REV7=ccccccc
NV_SIDECAR_ROOT=$HOME/sidecar
OPT_BUILD_SIDECAR_INTO=$HOME/out
OPT_DRY_RUN=1
sidecar_state() { NV_SIDECAR_PRESENT=0; }
curl_get() { echo unexpected-download >&2; exit 99; }
'''
    entry = {'install': 'ensure_sidecar', 'setup': 'do_sidecar_only', 'aur': 'do_build_sidecar_into'}[route]
    proc = run(script + entry + '\n', root)
    assert (proc.returncode == 0) == allowed, proc.stderr
    assert 'unexpected-download' not in proc.stderr
    if not fields:
        assert ('warning:' if allowed else 'refusing') in proc.stderr
    if not allowed:
        assert 'GRPC_TOOLS' in proc.stderr
        assert 'would build' not in proc.stdout


@pytest.mark.parametrize('route', ['install', 'setup', 'aur'])
@pytest.mark.parametrize('fields', ['', 'GRPC_TOOLS_VERSION=\n', pin('bad')])
def test_real_build_refuses_invalid_pin_before_acquiring_inputs(build, route, fields):
    root, sha, script = build
    (root / 'RELEASE').write_text(release(pin=fields))
    script += '''
parse_release "$HOME/RELEASE"
OPT_RELEASE_FILE=$HOME/RELEASE
NV_CACHE_NV=$HOME/cache
NV_SIDECAR_ROOT=$HOME/sidecar
sidecar_state() { NV_SIDECAR_PRESENT=0; }
require_curl() { :; }
check_base_url() { NV_BASE=https://example.invalid; }
acquire_lock() { :; }
check_sidecar_build_space() { :; }
sidecar_download_node() { echo unexpected-download >&2; exit 99; }
sidecar_download_verdandi_source() { echo unexpected-download >&2; exit 99; }
OPT_NODE_TARBALL=$HOME/missing-node.tgz
OPT_VERDANDI_SOURCE_TARBALL=$HOME/missing-source.tgz
OPT_BUILD_SIDECAR_INTO=$HOME/out
'''
    entry = {'install': 'install_sidecar_for_rev ccccccc 1', 'setup': 'do_sidecar_only',
             'aur': 'do_build_sidecar_into'}[route]
    proc = run(script + entry + '\n', root)
    assert proc.returncode != 0
    assert 'GRPC_TOOLS' in proc.stderr
    assert 'unexpected-download' not in proc.stderr
    assert 'no such file' not in proc.stderr
    assert not (root / 'cache').exists()


@pytest.mark.parametrize('route,shape', [('eitri-git', 'complete')] + [
    ('checkout', shape) for shape in ('complete', 'absent', 'version', 'x64', 'arm64', 'blank')])
def test_synthesized_release_copies_checked_in_compiler_pin(build, route, shape):
    root, sha, script = build
    tree = root / 'eitri-git'
    (tree / 'packaging').mkdir(parents=True)
    (tree / 'agent').mkdir()
    (tree / 'shell').mkdir()
    (tree / 'shell/Cargo.toml').write_text('[package]\nversion.workspace = true\n')
    (tree / '.git').mkdir()
    (root / 'verdandi').mkdir()
    fields = pin(sha)
    if shape == 'absent':
        fields = ''
    elif shape == 'blank':
        fields = 'GRPC_TOOLS_VERSION=\n'
    elif shape != 'complete':
        key = 'GRPC_TOOLS_VERSION' if shape == 'version' else f'GRPC_TOOLS_SHA256_linux_{shape}'
        fields = next(line + '\n' for line in fields.splitlines() if line.startswith(key + '='))
    (tree / 'packaging/pins.env').write_text(release(pin=fields))
    version = '0.3.1' if shape == 'complete' else '0.3.0'
    (tree / 'Cargo.toml').write_text(f'[workspace.package]\nversion = "{version}"\n')
    (tree / 'agent/Cargo.toml').write_text('claude-runtime-protocol = { git = "https://github.com/HunterGrey-cyber/verdandi", rev = "' + 'c' * 40 + '" }\n')
    git_stub = '''
git() {
    case "$*" in
        *rev-parse*) printf '%040d\n' 1 ;;
        archive*) cp "$HOME/source.tgz" "$4" ;;
    esac
}
'''
    if route == 'checkout':
        script += git_stub + '''
checkout_fork_rev() { CFR_REV=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; }
resolve_verdandi_source() { NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=$HOME/source.tgz; REL_VERDANDI_SOURCE_SHA256=dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd; }
NV_CACHE_NV=$HOME/cache
synthesize_release_from_checkout "$HOME/eitri-git"
cp "$NV_RELEASE_PATH" "$HOME/synthesized"
'''
    else:
        script = f'set -euo pipefail\nsource {shlex.quote(str(PACKAGING / "aur/eitri-git/PKGBUILD"))}\n' + git_stub + '''
srcdir=$HOME
_verdandi_node_pin() { echo v22.23.2; }
cargo() { :; }
rustc() { echo 'host: fixture'; }
curl() { :; }
sha256sum() { if [ "${1:-}" = -c ]; then cat >/dev/null; else command sha256sum "$@"; fi; }
prepare
cp "$srcdir/release-for-sidecar.env" "$HOME/synthesized"
'''
    proc = run(script, root, 'bash')
    assert proc.returncode == 0, proc.stderr
    synthesized = (root / 'synthesized').read_text()
    for line in fields.splitlines():
        assert line in synthesized.splitlines()
    if shape == 'absent':
        assert 'GRPC_TOOLS_' not in synthesized
    # Both the build and a later setup consume this same RELEASE.
    (root / 'RELEASE').write_text(synthesized)
    proc = run(installer() + 'parse_release "$HOME/RELEASE"\ncheck_bundled_protoc_pin\n', root)
    if shape in ('complete', 'absent'):
        assert proc.returncode == 0, proc.stderr
        assert ('warning:' in proc.stderr) == (shape == 'absent')
        if shape == 'absent':
            assert 'unverified compiler download' in proc.stderr
    else:
        assert proc.returncode != 0
        assert 'incomplete or malformed GRPC_TOOLS' in proc.stderr
        assert 'warning:' not in proc.stderr


def test_harness_releases_pin_fixture_compilers_and_keep_legacy_cases(tmp_path):
    scratch = tmp_path / '.cache/harness'
    # Only create fixtures: the supplied port prevents a server bind, and no test matches.
    proc = subprocess.run(['sh', str(PACKAGING / 'tests/install/harness.sh'), '--sh', 'bash',
                           '--scratch', str(scratch), '--server-port', '1', '--only', 'fixtures-only'],
                          capture_output=True, text=True, timeout=60)
    assert proc.returncode == 2 and 'no test ran' in proc.stderr, proc.stdout + proc.stderr
    for version in ('1.0.0', '1.1.0', '1.2.0', '1.0.0-rc.1'):
        text = (scratch / f'fix/v{version}/RELEASE').read_text()
        assert 'GRPC_TOOLS_VERSION=1.13.1' in text
        for arch in ('x64', 'arm64'):
            digest = hashlib.sha256((scratch / f'srv/grpc/grpc-tools/v1.13.1/linux-{arch}.tar.gz').read_bytes()).hexdigest()
            assert f'GRPC_TOOLS_SHA256_linux_{arch}={digest}' in text
    for version in ('0.3.0', '0.3.1'):
        assert 'GRPC_TOOLS_' not in (scratch / f'fix/v{version}/RELEASE').read_text()
