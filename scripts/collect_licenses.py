#!/usr/bin/env python3
"""Collect offline release notices for memq's default-feature Cargo graph.

Python 3.9+. Requires already-fetched Cargo sources. Normal and build edges
(including proc macros) are included conservatively; dev-only edges are excluded.
This is not a linker audit of Rust's standard library or system libraries.
Cargo metadata runs offline and does not execute dependency build scripts.

USearch 2.26.2 compiles its C++ headers and links the separate numkong crate,
not its repository's StringZilla submodule. SQLite's C dedication is separate
from libsqlite3-sys's MIT license. New native dependencies, build feature flags,
or source overrides require a fresh packaging review.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path, PureWindowsPath
import re
import shutil
import subprocess
import sys
import tempfile


class NoticeError(Exception):
    pass


NOTICE_NAME = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|unlicense)(?:$|[._-])", re.I)
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
# Full LICENSE files inspected at these published revisions, not SPDX substitutions:
# https://github.com/unum-cloud/USearch/blob/f91fe5bc000222aa1af6e91daf78c2bb20b0c90e/LICENSE
# https://github.com/ashvardanian/NumKong/blob/6de303be55d1957bc41c6d0d0ffd07c2e1627e43/LICENSE
# https://github.com/zurawiki/tiktoken-rs/blob/7c20dc69d6d71efceecd20daa7067fa92edea3ba/LICENSE
# The latter's vendor/tiktoken gitlink identifies this additional license:
# https://github.com/openai/tiktoken/blob/4560a8896f5fb1d35c6f8fd6eee0399f9a1a27ca/LICENSE
REVIEWED = {
    "usearch": ("2.26.2", "f91fe5bc000222aa1af6e91daf78c2bb20b0c90e", "apache"),
    "numkong": ("7.8.2", "6de303be55d1957bc41c6d0d0ffd07c2e1627e43", "apache"),
    "tiktoken-rs": ("0.7.0", "7c20dc69d6d71efceecd20daa7067fa92edea3ba", "tiktoken-rs"),
}
TEXT_HASHES = {
    "apache": "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4",
    "tiktoken-rs": "f7c6ddf9d84fd7b8ad5917e4074d4c05e4c1dfb752a28a0058f06bd0f5e2edcc",
    "tiktoken": "418cb499b436128d653d79941333a5437b7be2ea9213dcc2f04d15d5d2c51d86",
}

# Require substantive grants/conditions/disclaimers, not a name or URL.
# Unknown licenses need an explicitly reviewed recognizer.
LICENSE_MARKERS = {
    "MIT": ("permission is hereby granted, free of charge", "the above copyright notice",
            "shall be included", "the software is provided", "without warranty"),
    "Apache-2.0": ("apache license", "version 2.0", "grant of copyright license",
                   "grant of patent license", "limitation of liability", "end of terms and conditions"),
    "Zlib": ("permission is granted to anyone to use this software",
             "the origin of this software must not be misrepresented",
             "this notice may not be removed", "without any express or implied warranty"),
    "Unlicense": ("free and unencumbered software released into the public domain",
                  "anyone is free to copy", "the software is provided", "without warranty"),
    "BSL-1.0": ("boost software license - version 1.0", "permission is hereby granted",
                "the copyright notices", "the software is provided", "without warranty"),
    "BSD-2-Clause": ("redistribution and use in source and binary forms",
                     "redistributions of source code", "redistributions in binary form",
                     "this software is provided", "disclaimed"),
    "Unicode-3.0": ("unicode license v3", "permission is hereby granted",
                    "this copyright and permission notice",
                    "the data files and software are provided", "without warranty"),
}


def recognized(documents):
    found = set()
    for data in documents.values():
        text = " ".join(data.decode("utf-8-sig").lower().split())
        for license_id, markers in LICENSE_MARKERS.items():
            if all(marker in text for marker in markers):
                found.add(license_id)
    return found


def satisfies(expression, available):
    """Parse SPDX AND/OR/parentheses; legacy Cargo '/' means OR."""
    expression = expression.replace("/", " OR ")
    tokens = re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*|[()]", expression)
    if "".join(tokens) != re.sub(r"\s+", "", expression):
        raise NoticeError("unsupported license expression; review it before packaging")
    position = 0

    def atom():
        nonlocal position
        if position >= len(tokens):
            raise NoticeError("incomplete license expression")
        token = tokens[position]
        position += 1
        if token == "(":
            result = either()
            if position >= len(tokens) or tokens[position] != ")":
                raise NoticeError("unbalanced license expression")
            position += 1
            return result
        if token in ("AND", "OR", "WITH", ")"):
            raise NoticeError("unsupported license expression or exception")
        return token in available

    def both():
        nonlocal position
        result = atom()
        while position < len(tokens) and tokens[position] == "AND":
            position += 1
            following = atom()
            result = result and following
        return result

    def either():
        nonlocal position
        result = both()
        while position < len(tokens) and tokens[position] == "OR":
            position += 1
            following = both()
            result = result or following
        return result

    result = either()
    if position != len(tokens):
        raise NoticeError("unsupported license expression or exception")
    return result


def source_path(root, relative):
    relative = Path(relative)
    if relative.is_absolute() or ".." in relative.parts or PureWindowsPath(str(relative)).drive or "\\" in str(relative):
        raise NoticeError("absolute/traversing notice path; package notices inside the crate root")
    path = root
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            target = Path(os.readlink(path))
            if target.is_absolute() or ".." in target.parts or PureWindowsPath(str(target)).drive or "\\" in str(target):
                raise NoticeError("absolute/traversing notice symlink; package notices inside the crate root")
    try:
        resolved = path.resolve(strict=True)
    except (OSError, RuntimeError):
        raise NoticeError("missing or cyclic notice path; restore the package's Cargo source cache")
    if not resolved.is_relative_to(root.resolve()):
        raise NoticeError("notice escapes its package; include the actual text in the package")
    return resolved


def read_notice(root, relative):
    path = source_path(root, relative)
    try:
        data = path.read_bytes()
    except OSError:
        raise NoticeError("cannot read a required notice; restore the package's Cargo source cache")
    if not data.strip() or len(data) > 1024 * 1024 or b"\0" in data:
        raise NoticeError("empty, oversized, or binary notice; supply reviewed license text")
    try:
        data.decode("utf-8-sig")
    except UnicodeDecodeError:
        raise NoticeError("notice is not UTF-8; add a reviewed encoding conversion")
    return data


def package_documents(package):
    root = Path(package["manifest_path"]).parent
    documents = {}

    def scan_error(_error):
        raise NoticeError("cannot scan package notice directories; restore the Cargo source cache")

    for directory, directories, files in os.walk(root, followlinks=False, onerror=scan_error):
        directories[:] = sorted(d for d in directories if d not in (".git", "target"))
        # This crate also packages SQLCipher, which is not the supported build.
        if package["name"] == "libsqlite3-sys" and Path(directory) == root:
            directories[:] = [d for d in directories if d != "sqlcipher"]
        for filename in directories + files:
            path = Path(directory) / filename
            if path.is_symlink():
                source_path(root, path.relative_to(root))
        for filename in sorted(files):
            if NOTICE_NAME.match(filename):
                relative = (Path(directory) / filename).relative_to(root)
                documents[relative.as_posix()] = read_notice(root, relative)
    if package.get("license_file"):
        path = Path(package["license_file"])
        data = read_notice(root, path)
        if not recognized({path.as_posix(): data}):
            raise NoticeError("license_file lacks recognized license text; inspect it and supply complete terms")
        documents[path.as_posix()] = data
    return documents


def checked_text(kind):
    data = EMBEDDED_TEXTS[kind].encode("utf-8")
    if hashlib.sha256(data).hexdigest() != TEXT_HASHES[kind]:
        raise NoticeError("embedded license changed; restore its verified upstream text")
    return data


def require_reviewed_source(package):
    expected = REVIEWED.get(package["name"])
    if expected is None or package["version"] != expected[0] or package.get("source") != CRATES_IO:
        raise NoticeError("missing notices for an unreviewed version/source; inspect its license and vendor tree")
    try:
        root = Path(package["manifest_path"]).parent
        info = json.loads(source_path(root, ".cargo_vcs_info.json").read_text(encoding="utf-8"))
        matches = info["git"]["sha1"] == expected[1]
    except (OSError, ValueError, KeyError, TypeError):
        matches = False
    if not matches:
        raise NoticeError("source revision differs from the reviewed notice; inspect the published package")
    return expected[2]


def sqlite_notice(package, features, target):
    if any("sqlcipher" in feature for feature in features):
        raise NoticeError("SQLCipher is not reviewed; collect its native and crypto notices before release")
    bundled = "bundled" in features or ("bundled-windows" in features and "windows" in target)
    if not bundled:
        return None
    if "loadable_extension" in features or os.environ.get("LIBSQLITE3_SYS_USE_PKG_CONFIG", "0") != "0":
        raise NoticeError("SQLite linking override differs from the reviewed bundled build")
    header = source_path(Path(package["manifest_path"]).parent, "sqlite3/sqlite3.h")
    try:
        with header.open("rb") as source:
            data = source.read(65536)
    except OSError:
        raise NoticeError("bundled sqlite3/sqlite3.h is missing; restore the crate source")
    block = re.match(rb"\s*(/\*.*?\*/)", data, re.S)
    version = re.search(rb'#define SQLITE_VERSION\s+"([0-9.]+)"', data)
    if not block or not version or not all(text in block[1] for text in (
        b"The author disclaims copyright", b"May you do good and not evil.",
        b"May you find forgiveness", b"May you share freely",
    )):
        raise NoticeError("SQLite dedication/version changed; review its bundled source notice")
    return ("sqlite", version[1].decode("ascii"), "LicenseRef-SQLite-Public-Domain",
            {"PUBLIC-DOMAIN.txt": block[1] + b"\n"})


def dependencies(repo, target):
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
               "--filter-platform", target, "--manifest-path", str(repo / "Cargo.toml")]
    try:
        result = subprocess.run(command, cwd=repo, capture_output=True, text=True, check=False)
    except OSError:
        raise NoticeError("cargo is unavailable; put the release Rust toolchain on PATH")
    if result.returncode:
        # Cargo stderr can contain local paths or credentials; never publish it.
        raise NoticeError(
            "cargo metadata failed; check Cargo.toml/Cargo.lock and the Rust toolchain. "
            "Fetch sources with `cargo fetch --locked --target " + target + "` and retry. "
            "Run cargo metadata --locked --offline locally for the detailed diagnostic."
        )
    metadata = json.loads(result.stdout)
    resolve = metadata.get("resolve") or {}
    root = resolve.get("root")
    if not root or len(metadata["workspace_members"]) != 1:
        raise NoticeError("expected one Cargo root package; define binary selection before using a workspace")
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in resolve["nodes"]}
    seen, pending = set(), [root]
    while pending:
        package_id = pending.pop()
        if package_id in seen:
            continue
        seen.add(package_id)
        pending.extend(d["pkg"] for d in nodes[package_id]["deps"]
                       if any(k["kind"] in (None, "build") for k in d["dep_kinds"]))
    return [(packages[p], nodes[p]["features"]) for p in seen - {root}]


def collect(repo, target):
    snapshot = {name: (repo / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
    entries, errors, identities = [], [], set()
    for package, features in sorted(dependencies(repo, target),
                                    key=lambda item: (item[0]["name"], item[0]["version"])):
        name, version = package["name"], package["version"]
        label = name + "-" + version
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", label) or label in identities:
            raise NoticeError("ambiguous package name/version; review notice directory naming")
        identities.add(label)
        try:
            documents = package_documents(package)
            expression = package.get("license")
            available = recognized(documents)
            if expression and not satisfies(expression, available) and name in REVIEWED:
                documents["UPSTREAM-LICENSE.txt"] = checked_text(require_reviewed_source(package))
            if name == "tiktoken-rs":
                require_reviewed_source(package)
                documents["VENDORED-TIKTOKEN-LICENSE.txt"] = checked_text("tiktoken")
            available = recognized(documents)
            if not available or (expression and not satisfies(expression, available)):
                raise NoticeError(
                    "no complete text for declared license " + (expression or "(license-file)") +
                    "; restore packaged LICENSE/NOTICE files or add a version-pinned reviewed notice "
                    "(SPDX identifiers and URLs alone are insufficient)"
                )
            if not expression and not package.get("license_file"):
                raise NoticeError("package declares neither license nor license_file; inspect its license")
            entries.append((name, version, expression or "License file (no SPDX expression)", documents))
            if name == "libsqlite3-sys":
                native = sqlite_notice(package, features, target)
                if native:
                    entries.append(native)
        except NoticeError as error:
            errors.append(label + ": " + str(error))
    if errors:
        raise NoticeError("\n".join(errors))
    if any((repo / name).read_bytes() != data for name, data in snapshot.items()):
        raise NoticeError("Cargo.toml or Cargo.lock changed during collection; retry after dependency edits")
    return sorted(entries)


def write_bundle(output, entries, target):
    if output.is_symlink() or (output.exists() and (not output.is_dir() or any(output.iterdir()))):
        raise NoticeError("output must be absent or empty; use a fresh notice directory")
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=".license-stage-", dir=str(output.parent)))
    try:
        lines = ["# Third-party notices", "", "Target: `" + target + "`.", "",
                 "Default-feature Cargo normal/build closure; includes build tools and proc macros.",
                 "Dev-only dependencies, the root package, and toolchain/system libraries are excluded.",
                 "SQLite's dedication and the bundled USearch/NumKong and tiktoken code are included.",
                 "License texts and attributions are preserved; no Cargo metadata is published.", "",
                 "| Component | Version | Declared license | Notices |",
                 "| --- | --- | --- | --- |"]
        for name, version, expression, documents in entries:
            links = []
            for relative, data in sorted(documents.items()):
                path = Path(name + "-" + version) / relative
                if path.is_absolute() or ".." in path.parts:
                    raise NoticeError("unsafe notice output path; review package license_file")
                destination = stage / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                links.append("[" + relative + "](" + path.as_posix().replace(" ", "%20") + ")")
            lines.append("| " + " | ".join((name, version, expression, ", ".join(links))) + " |")
        (stage / "INDEX.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
        stage.rename(output)
    finally:
        if stage.exists():
            shutil.rmtree(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="Rust target triple used for the release build")
    parser.add_argument("--output", type=Path, required=True, help="Absent or empty destination directory")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-z0-9_]+(?:-[a-z0-9_]+){2,4}", args.target):
        parser.error("--target must be a Rust target triple, not a path or target JSON file")
    try:
        entries = collect(Path(__file__).resolve().parent.parent, args.target)
        write_bundle(args.output, entries, args.target)
    except (NoticeError, OSError, ValueError, KeyError) as error:
        # Unexpected filesystem/metadata errors must not dump absolute paths.
        detail = str(error) if isinstance(error, NoticeError) else "cannot read sources or write notices; check the Cargo cache and output permissions"
        print("collect_licenses: " + detail, file=sys.stderr)
        return 1
    print("Collected " + str(len(entries)) + " component notices for " + args.target + ".")
    return 0


MIT_GRANT = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""

EMBEDDED_TEXTS = {
    "tiktoken-rs": "MIT License\n\nCopyright (c) 2023 Roger Zurawicki\n\n" + MIT_GRANT,
    "tiktoken": "MIT License\n\nCopyright (c) 2022 OpenAI, Shantanu Jain\n\n" + MIT_GRANT,
    "apache": """                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
""",
}


if __name__ == "__main__":
    sys.exit(main())
