#!/usr/bin/env python3
"""Make a disposable, instrumented FilmCraft copy; never edit Cargo's checkout.
Usage: profile-filmcraft.py /path/to/pinned/filmcraft /private/tmp/new-directory
Then: cargo --config /private/tmp/new-directory/abner-profile.toml test ...
This is deliberately tied to Abner's pinned revision: substitutions fail on drift.
"""
import json
import pathlib
import shutil
import sys
import tomllib

source, dest = map(pathlib.Path, sys.argv[1:])
if dest.exists():
    raise SystemExit(f"destination must not exist: {dest}")
shutil.copytree(source, dest, ignore=shutil.ignore_patterns('.git', 'target'))


def edit(relative, old, new):
    path = dest / relative
    text = path.read_text()
    if text.count(old) != 1:
        raise SystemExit(f"expected exactly one instrumentation site: {relative}: {old}")
    path.write_text(text.replace(old, new))


vt = 'crates/platform/src/videotoolbox.rs'
edit(vt, '        match copy_out(pb, &layout) {', '''        let started = std::time::Instant::now();
        let copied = copy_out(pb, &layout);
        eprintln!("fc-profile callback_copy_including_pool_ns={}", started.elapsed().as_nanos());
        match copied {''')
edit(vt, '        unsafe { self.session.wait_for_asynchronous_frames() };', '''        let started = std::time::Instant::now();
        unsafe { self.session.wait_for_asynchronous_frames() };
        eprintln!("fc-profile asynchronous_wait_ns={}", started.elapsed().as_nanos());''')
# Separate pool mutex acquisition from fresh allocation calls. First-touch page
# faults happen later during copying and are not charged to allocation here.
edit('crates/frame/src/pool.rs',
     '    let found = shelf.lock().unwrap_or_else(PoisonError::into_inner).take(len);',
     '''    let started = std::time::Instant::now();
    let mut guard = shelf.lock().unwrap_or_else(PoisonError::into_inner);
    let lock_ns = started.elapsed().as_nanos();
    let found = guard.take(len);
    drop(guard);
    eprintln!("fc-profile plane_pool_lock_ns={lock_ns} reused={}", found.is_some());''')
edit('crates/frame/src/pool.rs', '        None => Vec::with_capacity(len),',
     '''        None => {
            let started = std::time::Instant::now();
            let buffer = Vec::with_capacity(len);
            eprintln!("fc-profile plane_allocation_ns={} bytes={}", started.elapsed().as_nanos(), len * std::mem::size_of::<T>());
            buffer
        },''')
edit('crates/codecs/src/gop.rs', '            self.store_output(st, out);\n            if st.cached(want_pts, draft).is_some() {', '''            let started = std::time::Instant::now();
            self.store_output(st, out);
            eprintln!("fc-profile gop_store_evict_ns={} retained_bytes={}", started.elapsed().as_nanos(), st.bytes);
            if st.cached(want_pts, draft).is_some() {''')
# Patch every package so workspace path dependencies and git dependencies have one identity.
lines = ['[patch."https://github.com/monomadic/filmcraft"]']
for manifest in sorted(dest.glob('crates/*/Cargo.toml')):
    name = tomllib.loads(manifest.read_text())['package']['name']
    lines.append(f'{json.dumps(name)} = {{ path = {json.dumps(str(manifest.parent))} }}')
config = dest / 'abner-profile.toml'
config.write_text('\n'.join(lines) + '\n')
print(config)
