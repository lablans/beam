# Samply.Broker
A distributed task broker designed for efficient communication across strict network environments.

## Getting started
Running the `central` binary will open a central broker instance listening on `0.0.0.0:8080`. The instance can be queried via the API (see next section).

## Data objects (JSON)
### Task
Tasks are represented in the following structure:

```json
{
  "id": "70c0aa90-bfcf-4312-a6af-42cbd57dc0b8",
  "to": [
    "6e3cf893-c134-45d2-b9f3-b02d92ad51e0",
    "0abd8445-b4a9-4e20-8a4a-bd97ed57745c"
  ],
  "task_type": "My important task",
  "body": "Much work to do",
  "failure_strategy": {
    "retry": {
      "backoff_millisecs": 1000,
      "max_tries": 5
    }
  }
}
```

- `id`: UUID to identify the task. When the task is initially created, the value is ignored and replaced by a server-generated one.
- `to`: UUIDs of *workers* allowed to retrieve the task and submit results.
- `task_type`: Well-known identifier for the type of work. Not interpreted by the Broker.
- `body`: Description of work to be done. Not interpreted by the Broker.
- `failure_strategy`: Advises each client how to handle failures. Possible values `discard`, `retry`.
- `failure_strategy.retry`: How often to retry (`max_tries`) a failed task and how long to wait in between each try (`backoff_millisecs`).

### Result
Each task can hold 0...n results by each *worker* defined in the task's `to` field.

A succeeded result for the above task:
```json
{
  "id": "8db76400-e2d9-4d9d-881f-f073336338c1",
  "worker_id": "6e3cf893-c134-45d2-b9f3-b02d92ad51e0",
  "task": "70c0aa90-bfcf-4312-a6af-42cbd57dc0b8",
  "result": {
    "succeeded": "All done!"
  }
}
```

A failed task:
```json
{
  "id": "24a49494-6a00-415f-80fc-b2ae34658b98",
  "worker_id": "0abd8445-b4a9-4e20-8a4a-bd97ed57745c",
  "task": "70c0aa90-bfcf-4312-a6af-42cbd57dc0b8",
  "result": {
    "permfail": "Unable to decrypt quantum state"
  }
}
```

- `id`: UUID identifying the result. When the result is initially created, the value is ignored and replaced by a server-generated one.
- `worker_id`: UUID identifying the client submitting this result. This needs to match an entry the `to` field in the task.
- `task`: UUID identifying the task this result belongs to.
- `result`: Defines status of this work result. Possible values `unclaimed`, `tempfail(body)`, `permfail(body)`, `succeeded(body)`.
- `result.body`: Either carries a return value (`succeeded`) or an error message.

## API
### Create task
Create a new task to be worked on by defined workers.

Method: `POST`  
URL: `/tasks`  
Body: see [Task](#task)  
Parameters: none

Returns:
```
HTTP/1.1 201 Created
location: /tasks/b999cf15-3c31-408f-a3e6-a47502308799
content-length: 0
date: Mon, 27 Jun 2022 13:58:35 GMT
```

In subsequent requests, use the URL defined in the `location` header to refer to the task (NOT the one you supplied in your POST body).

### Retrieve task
Workers regularly call this endpoint to retrieve submitted tasks.

Method: `GET`  
URL: `/tasks`  
Parameters:
- `worker_id` (optional): Fetch only tasks directed to this worker.
- [long polling](#long-polling) is supported.

Returns an array of tasks, cf. [here](#task)
```
HTTP/1.1 200 OK
content-type: application/json
content-length: 220
date: Mon, 27 Jun 2022 14:05:59 GMT

[
  {
    "id": ...
  }
)
```

### Retrieve results
The submitter of the task (see [Create Task](#create-task)) calls this endpoint to retrieve the results.

Method: `GET`  
URL: `/tasks/<task_id>/results`  
Parameters:
- [long polling](#long-polling) is supported.

Returns an array of results, cf. [here](#result)
```
HTTP/1.1 200 OK
content-type: application/json
content-length: 179
date: Mon, 27 Jun 2022 14:26:45 GMT

[
  {
    "id": ...
  }
]
```

## Long-polling API access
As part of making this API performant, all reading endpoints support long-polling as an efficient alternative to regular (repeated) polling. Using this function requires the following parameters:
- `poll_count`: The API call will block until this many results are available ...
- `poll_timeout`: ... or this many milliseconds have passed, whichever comes first.

For example, retrieving a task's results:
- `GET /tasks/<task_id>/results` will return immediately with however many results are available,
- `GET /tasks/<task_id>/results?poll_count=5` will block forever until 5 results are available,
- `GET /tasks/<task_id>/results?poll_count=5&poll_timeout=30000` will block until 5 results are available or 30 seconds have passed (whichever comes first). In the latter case, HTTP code 206 (Partial Content) is returned to indicate that the result is incomplete.

## To-Dos
- Make listen address & port configurable
- Authentication. This will change the API (e.g. `worker_id` will be no longer required as this is derived from the authentication)
- Additional client component to sign (and possibly encrypt) actual messages