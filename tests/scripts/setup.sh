#/bin/bash

eval $(cargo run --bin broker -- examples 2>/dev/null)

if [ "$?" != "0" ]; then
    echo -e "Failed to fetch examples; try running the following command and checking for errors:\n\ncargo run --bin central -- examples"
    exit 1
fi

SD=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
export WORKSPACE=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && cd ../../ && pwd )

export BROKER_URL="http://localhost:8080"
export BROKER_ID="localhost"
export PROXY_ID="proxy23.localhost"
export APP_ID="app1"
export PKI_ADDRESS="http://localhost:8200"
export PKI_APIKEY_FILE="$WORKSPACE/tests/pki_apikey.secret"
export PRIVKEY_FILE="$WORKSPACE/pki/proxy23.priv.pem"
export APPKEY_app1="MySecret"

export P="http://localhost:8081" # for scripts

TARGET_DIR=$(cat ~/.cargo/config.toml | grep "^target-dir" |sed 's;.*\"\(.*\)\".*;\1;g')
if [ -z $TARGET_DIR ]; then
    TARGET_DIR=target
fi

cd $WORKSPACE

function start {
    pki/pki devsetup &
    while ! openssl rsa -in $PRIVKEY_FILE -noout -check >/dev/null 2>/dev/null; do 
        sleep 0.1
    done
    cargo build 2>/dev/null
    $TARGET_DIR/debug/proxy &
    $TARGET_DIR/debug/broker &
}

function stop {
    killall vault proxy broker
}

function clean {
    pki/pki clean
}

function testing {
    echo "TEST \"$@\""
}

function fail {
    echo "FAIL $@"
    exit 1
}

function success {
    echo "  OK"
}

#function curl_get_raw {
#    curl -H "content-type: application/json" -H "Authorization: ApiKey $APP_ID.$PROXY_ID MySecret" $@
#}

#function curl_post_raw {
#    curl -H "content-type: application/json" -H "Authorization: ApiKey $APP_ID.$PROXY_ID MySecret" -d @- $@
#}

function curl_get {
    BODY_FILE=$(mktemp)
    JSON=$(curl -s -H "content-type: application/json" -H "Authorization: ApiKey $APP_ID.$PROXY_ID MySecret" -w %{json} -o $BODY_FILE $@)
    BODY=$(cat $BODY_FILE | sed 's/\"/\\\"/g')
    JSON=$(echo "$JSON" | jq ". + {\"body\": \"${BODY}\" }")
    rm $BODY_FILE
    echo "$JSON"
}

function curl_post {
    BODY_FILE=$(mktemp)
    JSON=$(curl -s -H "content-type: application/json" -H "Authorization: ApiKey $APP_ID.$PROXY_ID MySecret" -d @- -w %{json} -o $BODY_FILE $@)
    BODY=$(cat $BODY_FILE | sed 's/\"/\\\"/g')
    JSON=$(echo "$JSON" | jq ". + {\"body\": \"${BODY}\" }")
    rm $BODY_FILE
    echo "$JSON"
}

export -f curl_get curl_post start stop clean testing fail success
