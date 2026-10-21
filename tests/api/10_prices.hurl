# What each model costs, set and read back. Prices are the deployment's rather than any
# tenant's, so only the system administrator reaches them.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# Micros of the deployment's currency per million tokens: three units in, fifteen out.
PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "claude-opus-5", "input": 3000000, "output": 15000000}
HTTP 201
[Asserts]
jsonpath "$.model" == "claude-opus-5"
jsonpath "$.input" == 3000000
jsonpath "$.output" == 15000000
jsonpath "$.created_at" isInteger

PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "claude-haiku-5-5", "input": 250000, "output": 1250000}
HTTP 201

GET {{host}}/_ip/prices
HTTP 200
[Asserts]
jsonpath "$" count == 2

# One model holds one price: setting it again replaces what it cost.
PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "claude-opus-5", "input": 1000000, "output": 5000000}
HTTP 201
[Asserts]
jsonpath "$.input" == 1000000

GET {{host}}/_ip/prices
HTTP 200
[Asserts]
jsonpath "$" count == 2

# A rate with the decimal point in the wrong place is refused by the type.
PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "claude-opus-5", "input": 9000000000000, "output": 1}
HTTP 422

# So is a name that is not a model's.
PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "", "input": 1, "output": 1}
HTTP 422

# A model that costs nothing is priced at nothing, which is not the same as unpriced.
PUT {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "local-llama", "input": 0, "output": 0}
HTTP 201
[Asserts]
jsonpath "$.input" == 0

DELETE {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "local-llama"}
HTTP 204

# Unpricing a model that nothing prices says so rather than passing silently.
DELETE {{host}}/_ip/prices
x-ip-csrf: 1
{"model": "local-llama"}
HTTP 404

GET {{host}}/_ip/prices
HTTP 200
[Asserts]
jsonpath "$" count == 2
