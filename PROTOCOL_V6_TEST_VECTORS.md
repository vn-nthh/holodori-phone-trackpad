# Protocol v6 test vectors

These vectors freeze the byte-level interoperability contract between the Rust
host and Android implementations of [protocol v6](PROTOCOL_V6.md). All
integers in protocol headers are little endian, all hexadecimal strings are
contiguous bytes, and all private keys in this file are test data that MUST NOT
be used outside tests.

The executable copies live in:

- `native-host/src/secure_host.rs`, test `published_protocol_vectors_are_stable`;
- `android-app/app/src/test/java/dev/holodori/trackpad/SecureProtocolTest.java`,
  test `noiseAndRecordVectorsMatchPublishedRustBytes`; and
- `native-host/src/secure.rs` for authenticated replay rejection and touch
  record parsing.

Every value that depends on the Noise prologue differs from
[the v5 vectors](PROTOCOL_V5_TEST_VECTORS.md); the static keys, ephemeral keys,
and XX message 1 do not.

## Fixed inputs

```text
XX exchange ID     = 000102030405060708090a0b0c0d0e0f
host static private= 101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f
phone static private=303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f
host XX ephemeral  = 505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f
phone XX ephemeral = 707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f
phone SAS random   = a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf
host SAS random    = c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf
IK exchange ID     = f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff
phone IK ephemeral = 909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeaf
host IK ephemeral  = b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecf
session parameters = 8000000006000000 0102030405060708
```

The session parameters are a receive window of 128, lane count 6, zero flags
and reserved bytes, and host time `0x0807060504030201`.

The derived X25519 public keys are:

```text
host  = d89e3bad79437dbed9f843418304f460ff05c7fe81fe4a9577a804cb9367ff66
phone = 34e42d4af5ef94a07a3a84201b889d4cd1a743cb27b11b6a10438a8feb8e5847
```

## Noise XX pairing

Pattern: `Noise_XX_25519_ChaChaPoly_BLAKE2s`. The host is the initiator, the
phone is the responder, and the selected transport is Wi-Fi (`0x02`).

```text
prologue = 686f6c6f646f72692d70686f6e652d747261636b7061642d76360002000102030405060708090a0b0c0d0e0f
m1 = 392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c
m2 = 23b7bb8c91ae008711fb12846780bcdf1e065f821bdfec49f57e7c7dcd4c4823f56b5ed019d6b4f7d390bd2416f19670654ee0fdcfd6a275323659d4bc92bd3beac7cf12d97459e8bce0f17b8686e1e9c32d8910b963c3f7ab6b51b3fdda186a
m3 = f531830cca96c417accf9c7fbb8b15f7eb91cc4ec6e41d779f704ed44dc67f663bdd1c6d809eb4c5cea8fcc8b37a03f7a58efc53a85d46c286b57342424311c1
handshake hash = 6977c5b56b759d9a520bd729380074a5613eae3bf5fe5642158d81e2b9f1b50a
```

The raw Noise messages above are the payloads of HPP6 steps 1, 2, and 3. For
example, the complete step-1 `PAIR_OFFER` datagram is:

```text
4850503606024000000102030405060708090a0b0c0d0e0f0100000020000201392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c
```

## Commitment and six-lane comparison

Commitments use
`BLAKE2s("holodori-v5-sas-commit" || role || handshake_hash || random)`,
where phone role is `0x01` and host role is `0x02`. The label keeps its v5
spelling; the v6 prologue already separates the transcripts.

```text
phone commitment = c6b017e01832c9d7abf45934974870e4a2da962b8e83575607b3b0dcb10a3735
host commitment  = ab551195b25ad3b763aa5f3feb8ae61097263247319d53b545cdb89ae7d319c5
SAS digest       = f0ab28ccbe90d24214154c1d06696aeaa4d09e1ef9721261857a9f5256da1a5b
lane pattern     = 2, 5, 2, 5, 5, 4, 1, 5
```

The SAS digest is
`BLAKE2s("holodori-v5-sas" || handshake_hash || phone_random || host_random)`.
Lane mapping applies the rejection-sampling algorithm in `PROTOCOL_V6.md` and
returns one-based lane numbers.

## Authenticated record and nonce progression

After the XX split, the phone encrypts message type `PHONE_SAS_REVEAL` (`0x04`)
with session ID zero, logical ID 9, flags zero, and plaintext ASCII `vector`.
The entire 48-byte HPT6 header is associated data.

Packet number 0, nonce `000000000000000000000000`:

```text
4850543606044600326fc09cffc2e6c20000000000000000000000000000000009000000000000000000000006000000eae096ab93850975f72cbd58bf71cc41c31a0dea4fae
```

The same logical record sent again MUST burn packet number 1 and use nonce
`000000000100000000000000`:

```text
4850543606044600326fc09cffc2e6c2000000000000000001000000000000000900000000000000000000000600000033de984324ab761f90dcad0d39c3b93adcfaeebc7fd0
```

The packet number and ciphertext/tag both change; session ID, logical ID,
flags, and plaintext do not.

## Noise IK remembered session

Pattern: `Noise_IK_25519_ChaChaPoly_BLAKE2s`. The phone is the initiator, the
host is the responder, and the selected transport is USB (`0x01`). IK message 1
has an empty payload; IK message 2 encrypts the session parameters above.

```text
prologue = 686f6c6f646f72692d70686f6e652d747261636b7061642d76360001f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff
m1 = 9fd7ad6dcff4298dd3f96d5b1b2af910a0535b1488d7f8fabb349a982880b615ea374cd73714b7bd8d86c36ef4edda85485b3a2b38748dff758fd6ec58a7fb5a8a8e41171e51f08d93b6aa549f14cc552d48f941f2db491d8f61982720b5602d
m2 = 3f3e5f6d86926c9c128cf84581574f96840d98ee5ab53b1ec3b76e2bb25b945e512fc5d532d298f88d3122fadeced96e315f4782fef1855fabe09742b9d4dbf7
handshake hash = 45f5c953a9233003fc076d6d069257ec76a04d65af607be74cb70246411d10e7
```

## Replay and corrupt-tag outcomes

Use a fresh receiver for each case:

1. Open packet 1, then packet 0: both are accepted, proving bounded
   authenticated reordering.
2. Open either exact packet twice: the second open returns `Replay` and never
   reaches the logical-frame reorder buffer.
3. Change the final byte of packet 0 from `ae` to `af`: opening returns
   `BadTag`. Opening the original exact packet 0 afterward succeeds, proving an
   unauthenticated packet cannot consume a replay-window position.
4. After accepting a packet more than 1,023 positions newer, an unseen older
   packet outside the 1,024-packet window returns `Replay`.

The Rust and Android suites also assert that packet copies with unchanged
logical IDs always consume distinct packet numbers.
