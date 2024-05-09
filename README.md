# ldk-sample

Sample node implementation using LDK.

## Installation

```
git clone https://github.com/lightningdevkit/ldk-sample
```

## Usage

```
cd ldk-sample
cargo run <bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port> <ldk_storage_directory_path> <private-key> [<ldk-peer-listening-port>] [<bitcoin-network>] [<announced-node-name>] [<b>] [<announced-listen-addr>]
```

`bitcoind`'s RPC username and password likely can be found through `cat ~/.bitcoin/.cookie`.

`bitcoin-network`: defaults to `testnet`. Options: `testnet`, `regtest`, and `signet`.

`ldk-peer-listening-port`: defaults to 9735.

`announced-listen-addr` and `announced-node-name`: default to nothing, disabling any public
announcements of this node.
`announced-listen-addr` can be set to an IPv4 or IPv6 address to announce that as a
publicly-connectable address for this node.
`announced-node-name` can be any string up to 32 bytes in length, representing this node's alias.

## License

Licensed under either:

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)
  or http://www.apache.org/licenses/LICENSE-2.0)
* MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## For example, settuping 4 nodes:

``` shell
cargo install --path .
```

Alice:

```shell
yuv-ln-node admin1:123@127.0.0.1:18443 ./volumes.dev/8006/ cQb7JarJTBoeu6eLvyDnHYNr6Hz4AuAnELutxcY478ySZy2i29FA 8006 regtest yuv-node-8006 http://127.0.0.1:18333 127.0.0.1:8006
```

Bob:

``` shell
yuv-ln-node admin1:123@127.0.0.1:18443 ./volumes.dev/8007/ cUrMc62nnFeQuzXb26KPizCJQPp7449fsPsqn5NCHTwahSvqqRkV 8007 regtest yuv-node-8007 http://127.0.0.1:18335 127.0.0.1:8007
```

Carol:

``` shell
yuv-ln-node admin1:123@127.0.0.1:18443 ./volumes.dev/8008/ cUrvVCz1YKAwX6JLRhvacMWtkiLxHkTZbqEXu64Q4KySPcVrXuLu 8008 regtest yuv-node-8008 http://127.0.0.1:18336 127.0.0.1:8008
```

Dan:

``` shell
yuv-ln-node admin1:123@127.0.0.1:18443 ./volumes.dev/8009/ cUCcbqBwTHBH98qH4ghfP4DW6nHvh8VSuqiSveXSRgHyNVLAKXnn 8009 regtest yuv-node-8009 http://127.0.0.1:18333 127.0.0.1:8009
```

Connect Alice, Bob, Carol and Alice between each other.

```
connectpeer 02f338eea03393497c2bd2bf0780398d0e4f23ac1e99d1c899f1f7553fe01b54dc@127.0.0.1:8006
connectpeer 033ca6472b079e7c377d94d5b776225a7288e23685ce158b005c25b837c036b995@127.0.0.1:8007
connectpeer 03a581cf044380f45726f1174ba245bccdbbdc1617bae8ee96f96c63b6d75e1429@127.0.0.1:8008
connectpeer 0288483bd20d51f714e3faa70a813cdbf6b3c45121f1bf9c13dce0f6b8407d38b5@127.0.0.1:8009
```

Open channels Alice -> Bob -> Carol -> Dan (USD):

```
openchannel 033ca6472b079e7c377d94d5b776225a7288e23685ce158b005c25b837c036b995 200000 --pixel 6000:bcrt1p4v5dxtlzrrfuk57nxr3d6gwmtved47ulc55kcsk30h93e43ma2eqvrek30 --public
```

```
openchannel 03a581cf044380f45726f1174ba245bccdbbdc1617bae8ee96f96c63b6d75e1429 200000 --pixel 6000:bcrt1p4v5dxtlzrrfuk57nxr3d6gwmtved47ulc55kcsk30h93e43ma2eqvrek30 --public
```

```
openchannel 0288483bd20d51f714e3faa70a813cdbf6b3c45121f1bf9c13dce0f6b8407d38b5 200000 --pixel 6000:bcrt1p4v5dxtlzrrfuk57nxr3d6gwmtved47ulc55kcsk30h93e43ma2eqvrek30 --public
```

Get invoice from Dan:

```
getinvoice 15000000 36000 --pixel 500:bcrt1p4v5dxtlzrrfuk57nxr3d6gwmtved47ulc55kcsk30h93e43ma2eqvrek30
```

Copy it and insert for Alice:

```
sendpayment <copied invoice>
```
