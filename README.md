# evdi

[![Crates.io](https://img.shields.io/crates/v/evdi)](https://crates.io/crates/evdi)

High level bindings to [evdi](https://github.com/DisplayLink/evdi), a program for managing virtual
displays on linux.

Warning: This is alpha-quality software. If it breaks something maybe try
rebooting your computer.

Evdi doesn't full document the invarients necessary for memory safety, so I'm not confident this library is sound yet.

See also the low-level unsafe bindings [evdi-sys](https://crates.io/crates/evdi-sys),
and [the docs](https://displaylink.github.io/evdi/) for libevdi.
