#!/bin/bash -e

SD=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )

source $SD/setup.sh

#start

testing Create task
RET=$(echo $TASK0 | curl_post $P/v1/tasks)

CODE=$(echo $RET | jq -r .response_code)

if [ "$CODE" != "201" ]; then
    fail Expected code 201, got $CODE
fi

success
