# Storing wasm and placing it in the chain the caller manages.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

POST {{host}}/_ip/plugins?kind=resp_body
x-ip-csrf: 1
file,{{plugin}};
HTTP 201
[Captures]
checksum: jsonpath "$.checksum"
[Asserts]
jsonpath "$.kind" == "resp_body"
jsonpath "$.size" > 0
jsonpath "$.created_at" isInteger

# Wasm the host will not load is never stored.
POST {{host}}/_ip/plugins?kind=resp_body
x-ip-csrf: 1
```
not wasm at all
```
HTTP 422

GET {{host}}/_ip/plugins
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].checksum" == "{{checksum}}"

# The system administrator's chain is the global one, at the primary orders.
POST {{host}}/_ip/plugin-rules
x-ip-csrf: 1
{"checksum": "{{checksum}}", "order": 10}
HTTP 201
[Asserts]
jsonpath "$.kind" == "resp_body"
jsonpath "$.order" == 10

POST {{host}}/_ip/plugin-rules
x-ip-csrf: 1
{"checksum": "{{checksum}}", "order": 100}
HTTP 422

GET {{host}}/_ip/plugin-rules
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].order" == 10
jsonpath "$[0].created_at" isInteger

# A plugin a chain still runs does not go.
DELETE {{host}}/_ip/plugins/{{checksum}}
x-ip-csrf: 1
HTTP 409

DELETE {{host}}/_ip/plugin-rules/resp_body/10
x-ip-csrf: 1
HTTP 204

DELETE {{host}}/_ip/plugins/{{checksum}}
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/plugins
HTTP 200
[Asserts]
jsonpath "$" count == 0
