# Protocol V5 test vectors

These vectors freeze the byte-level interoperability contract between the Rust
host and Android implementations. All integers in protocol headers are little
endian, all hexadecimal strings are contiguous bytes, and all private keys in
this file are test data that MUST NOT be used outside tests.

The executable copies live in:

- `native-host/src/v5_host.rs`, test
  `published_protocol_vectors_are_stable`;
- `android-app/app/src/test/java/dev/holodori/trackpad/V5ProtocolTest.java`,
  test `noiseAndRecordVectorsMatchPublishedRustBytes`; and
- `native-host/src/v5.rs` for authenticated replay rejection.

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
```

The derived X25519 public keys are:

```text
host  = d89e3bad79437dbed9f843418304f460ff05c7fe81fe4a9577a804cb9367ff66
phone = 34e42d4af5ef94a07a3a84201b889d4cd1a743cb27b11b6a10438a8feb8e5847
```

## Noise XX pairing

Pattern: `Noise_XX_25519_ChaChaPoly_BLAKE2s`. The host is the initiator, the
phone is the responder, and the selected transport is Wi-Fi (`0x02`).

```text
prologue = 686f6c6f646f72692d70686f6e652d747261636b7061642d76350002000102030405060708090a0b0c0d0e0f
m1 = 392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c
m2 = 23b7bb8c91ae008711fb12846780bcdf1e065f821bdfec49f57e7c7dcd4c4823f56b5ed019d6b4f7d390bd2416f19670654ee0fdcfd6a275323659d4bc92bd3bbfa33a1e12cb80ccbaa5fe3be21e12a6cf4b9a56b3cdc11bcb166b362cb1b576
m3 = f531830cca96c417accf9c7fbb8b15f7eb91cc4ec6e41d779f704ed44dc67f66d8795cbaffa82eeb78befae0e0cde6c0d922ad90d8718e5c88d2cdcb78ed9563
handshake hash = bbd8c76e72aba9685e6855cc0862de61d1d01529342cb8987f23c9a8b65e647e
```

The raw Noise messages above are the payloads of HPP5 steps 1, 2, and 3. For
example, the complete step-1 `PAIR_OFFER` datagram is:

```text
4850503505024000000102030405060708090a0b0c0d0e0f0100000020000201392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c
```

## Commitment and six-lane comparison

Commitments use
`BLAKE2s("holodori-v5-sas-commit" || role || handshake_hash || random)`,
where phone role is `0x01` and host role is `0x02`.

```text
phone commitment = ea96f02ca5508df65cbe4c43e2b03fc4684bb9be33411f3aaa38edc625c112ca
host commitment  = 3156cb718322b5695a70a7a4d4097dabf1ffce6f2a5d5bc0fd779617bc10e698
SAS digest       = 95fedb94b066e0ec093efcd8026c8a8d82200248804fa15a8f372d1fc42bbea7
lane pattern     = 6, 3, 6, 4, 2, 5, 5, 6
```

The SAS digest is
`BLAKE2s("holodori-v5-sas" || handshake_hash || phone_random || host_random)`.
Lane mapping applies the rejection-sampling algorithm in `PROTOCOL_V5.md` and
returns one-based lane numbers.

## Authenticated record and nonce progression

After the XX split, the phone encrypts message type `PHONE_SAS_REVEAL` (`0x04`)
with session ID zero, logical ID 9, flags zero, and plaintext ASCII `vector`.
The entire 48-byte HPT5 header is associated data.

Packet number 0, nonce `000000000000000000000000`:

```text
4850543505044600b8be365398adc6fe0000000000000000000000000000000009000000000000000000000006000000eae096ab9385ca84ff8fd2b82c4de6cc4890137c4c0d
```

The same logical record sent again MUST burn packet number 1 and use nonce
`000000000100000000000000`:

```text
4850543505044600b8be365398adc6fe000000000000000001000000000000000900000000000000000000000600000033de984324ab289c9dd1f981e60265f9ff97e2d743e6
```

The packet number and ciphertext/tag both change; session ID, logical ID,
flags, and plaintext do not.

## Noise IK remembered session

Pattern: `Noise_IK_25519_ChaChaPoly_BLAKE2s`. The phone is the initiator, the
host is the responder, and the selected transport is USB (`0x01`).

```text
prologue = 686f6c6f646f72692d70686f6e652d747261636b7061642d76350001f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff
m1 = 9fd7ad6dcff4298dd3f96d5b1b2af910a0535b1488d7f8fabb349a982880b615ea374cd73714b7bd8d86c36ef4edda85485b3a2b38748dff758fd6ec58a7fb5a742888fec59468946610d729351f3f31f7693e1d35a73a19431d9b717c57d0fb
m2 = 3f3e5f6d86926c9c128cf84581574f96840d98ee5ab53b1ec3b76e2bb25b945ed563e952a259dcdc24aab223c0760b12
handshake hash = 217b487f44138992d172c6902fc2ba17c08d0205cb11c9b2e209f9aeeffaf3a8
```

## Replay and corrupt-tag outcomes

Use a fresh receiver for each case:

1. Open packet 1, then packet 0: both are accepted, proving bounded
   authenticated reordering.
2. Open either exact packet twice: the second open returns `Replay` and never
   reaches the logical-frame reorder buffer.
3. Change the final byte of packet 0 from `0d` to `0c`: opening returns
   `BadTag`. Opening the original exact packet 0 afterward succeeds, proving an
   unauthenticated packet cannot consume a replay-window position.
4. After accepting a packet more than 1,023 positions newer, an unseen older
   packet outside the 1,024-packet window returns `Replay`.

The Rust and Android suites also assert that packet copies with unchanged
logical IDs always consume distinct packet numbers.
