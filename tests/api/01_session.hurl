# Logging in, carrying the session, and giving it up.

# Every refusal is the same status: telling a wrong user from a wrong passphrase would say
# which accounts exist.
POST {{host}}/_ip/login
{"user": "root", "passphrase": "not the passphrase"}
HTTP 401

POST {{host}}/_ip/login
{"user": "nobody", "passphrase": "{{passphrase}}"}
HTTP 401

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204
[Asserts]
header "set-cookie" contains "ip_session="
header "set-cookie" contains "HttpOnly"
header "set-cookie" contains "SameSite=Strict"
header "set-cookie" contains "Path=/"

GET {{host}}/_ip/session
HTTP 200
[Asserts]
jsonpath "$.user" == "root"
jsonpath "$.system_administrator" == true

# A cookie alone changes nothing: a request that changes something must carry the header no
# other site can make a browser send.
POST {{host}}/_ip/logout
HTTP 403

POST {{host}}/_ip/logout
x-ip-csrf: 1
HTTP 204

# The token is refused for as long as it would have been good for.
GET {{host}}/_ip/session
HTTP 401
