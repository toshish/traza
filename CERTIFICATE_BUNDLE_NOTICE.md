# Certificate bundle notice

The release container includes Mozilla CA certificate data at
`/etc/ssl/certs/ca-certificates.crt`, copied unchanged from Alpine Linux
`ca-certificates-bundle` version `20260611-r0`. The certificate data is
covered by the Mozilla Public License 2.0; a copy is in [MPL-2.0.txt](MPL-2.0.txt).

The corresponding source, including `certdata.txt`, is available without
charge in the [Alpine ca-certificates 20260611 source archive](https://gitlab.alpinelinux.org/alpine/ca-certificates/-/archive/20260611/ca-certificates-20260611.tar.bz2).
The archive is identified by this SHA512, recorded in Alpine's
[package recipe](https://raw.githubusercontent.com/alpinelinux/aports/3.24-stable/main/ca-certificates/APKBUILD):

```
f473a1111eb508ef5d1096489479dba07db6b0d76ef2b900b3473933d57419429061d83e9c1e881ab61c4b0f17c64859ec74d670da6fc1d83d9b0fa73e1b7d8d
```

The source certificate file carries this notice:

> This Source Code Form is subject to the terms of the Mozilla Public
> License, v. 2.0. If a copy of the MPL was not distributed with this
> file, You can obtain one at http://mozilla.org/MPL/2.0/.

Alpine identifies the complete package as `MPL-2.0 AND MIT`. Its certificate
management programs, including the MIT-licensed helper by Timo Teräs, are
build-stage tools and are not included in Traza's scratch image.

The release build obtains this bundle from the pinned
`alpine:3.24.1` image index
`sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b`.
The bundle is 179,359 bytes with SHA256:

```
b8d837841b88bfaa1a0fa827cbca8e2576418dd47c9fc4bb7f1f9d89c83111b9
```

Native release archives use the host operating system's certificate trust
store. This notice describes the additional bundle shipped in the container.
