# Third-party notices

This project contains work derived from, or built on, the following
projects.  Their notices apply to the parts they contributed.

## TuxMix — mixer application

<https://github.com/ismail-bahloul/TuxMix>

MIT License, Copyright (c) 2026 Iswad.  The license and copyright notice are
kept verbatim in [`ui/tuxmix/LICENSE`](ui/tuxmix/LICENSE).

## snd-fireface-uc — Fireface UC kernel driver

<https://github.com/berndie/snd-fireface-uc>

Repository license: MIT, Copyright (c) 2026 Bernd Accou.  The Fireface UC
transport in `kernel/` follows this driver's device modes, packet layout,
rate table and status path.

```text
MIT License

Copyright (c) 2026 Bernd Accou

Permission is hereby granted, free of charge, to any person obtaining a copy
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
```

Note: the source files in that project carry `GPL-2.0-only` SPDX headers
while the repository license is MIT.  Both statements are reproduced here.

## rme-fireface-ucx-linux-driver — Fireface UCX kernel driver

<https://github.com/OroboroModular/rme-fireface-ucx-linux-driver>

GPL-2.0-or-later (SPDX headers).  We use it under GPL-2.0, which is why the
kernel module in this repository is GPL-2.0-only.  The Fireface UCX transport,
register model and meter decoding in `kernel/` build on this project.

## Reference material

Protocol facts were checked against RME USB captures and against the public
reverse-engineering notes of
[agfline/RME-Fireface-UC-Drivers](https://github.com/agfline/RME-Fireface-UC-Drivers),
which carries no license and is used as a factual reference only; no code
from it is included.
