#!/usr/bin/env bash
# 
# This scripts starts node for some user, using predefined from
# test_data dir seeds.

set -e

declare -A users_keys_map=(
 ["alice"]="cQb7JarJTBoeu6eLvyDnHYNr6Hz4AuAnELutxcY478ySZy2i29FA"
 ["bob"]="cUrMc62nnFeQuzXb26KPizCJQPp7449fsPsqn5NCHTwahSvqqRkV"
 ["carol"]="cPfW9LJ8ffX9vykELqc7KyzkqeXmQhRiajQQESj4uE9UzHNWrFrx"
 ["dan"]="cUkaaPEAvEjqRqF4zktqw6VHVkFgRcuJdti7Fe7YcdRMu3CJfFKH"
)

declare -A _users_node_id_map=(
 ["alice"]="03419cc4ce1b87e1b4f97ded6721f4a516fc29baaa39ac400a0f8a3eaaf418230c"
 ["bob"]="03de26e88fbc9d13470eeb62fd0ffc436bee42ea8c1a00651ed31d2385becab912"
 ["carol"]="03a5a5ed3baef38531793ab1febc6ec015125c5245d09354ba962438ede0469918"
 ["dan"]="0269649f8d87d77041da5f6bb6921f322633fafd22a525201ef2a0de3a63530679"
)

declare -A users_ports_map=(
 ["alice"]=8006
 ["bob"]=8007
 ["carol"]=8008
 ["dan"]=8009
)

#required
user="$1"
#optional
data_dir="$2"
bitcoin_url="$3"
yuv_url="$4"

if [[ -z ${user} ]]; then 
  # Check this name exists in map
  if [[ -z "${users_keys_map[${user}]}" ]]; then
    echo "${user} is not one of: alice, bob, carol, dan"
  else
    echo "set first parameter with one of: alice, bob, carol, dan"
  fi
  exit 1
fi

# for optionals, if not set, choose default value

if [[ -z ${data_dir} ]]; then
  data_dir="$PWD/volumes.dev"
fi

if [[ -z ${bitcoin_url} ]]; then
  bitcoin_url="admin1:123@127.0.0.1:18443"
fi

if [[ -z ${yuv_url} ]]; then
  yuv_url="http://127.0.0.1:18333"
fi

ldk_dir="${data_dir}/${user}"

# initialize dir to paste seed there
mkdir -p "${ldk_dir}/.ldk/"

seed_path="$PWD/test_data/${user}_seed"
if [[ -f ${seed_path} ]]; then
  cp "$seed_path" "${ldk_dir}/.ldk/keys_seed"
fi

private_key="${users_keys_map[${user}]}"
port="${users_ports_map[${user}]}"

set +e

yuv-ln-node "$bitcoin_url" "$ldk_dir" "$private_key" "$port" regtest "$user" "$yuv_url" "127.0.0.1:$port"
