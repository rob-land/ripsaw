#!/usr/bin/env python3
# Run `cargo build` and copy the resulting binary to the path Meson expects.
#
# Meson's custom_target declares its output ('ripsaw') under the target's
# private build directory, but cargo writes to its own --target-dir. Without
# this bridge the build succeeds yet `meson install` fails with
# "File 'src/ripsaw' could not be found". This wrapper runs cargo and then
# copies the freshly built binary to @OUTPUT@.

import os
import shutil
import subprocess
import sys


def main() -> int:
    cargo, manifest, target_dir, subdir, output = sys.argv[1:6]

    cmd = [cargo, 'build', '--manifest-path', manifest,
           '--target-dir', target_dir]
    if subdir == 'release':
        cmd.append('--release')

    subprocess.run(cmd, check=True)

    binary = os.path.join(target_dir, subdir, 'ripsaw')
    shutil.copy2(binary, output)
    return 0


if __name__ == '__main__':
    sys.exit(main())
