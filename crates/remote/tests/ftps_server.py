# An explicit FTPS server for the tests (pyftpdlib + pyOpenSSL): serves a folder to user
# "ana", password "s3nh@ forte", with the certificate and key given, TLS required on both the
# control and the data connections.
#
#   python ftps_server.py <folder> <port> <cert.pem> <key.pem>
import sys

from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import TLS_FTPHandler
from pyftpdlib.servers import FTPServer

folder, port, cert, key = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
auth = DummyAuthorizer()
auth.add_user("ana", "s3nh@ forte", folder, perm="elradfmwMT")
handler = TLS_FTPHandler
handler.certfile = cert
handler.keyfile = key
handler.authorizer = auth
handler.tls_control_required = True
handler.tls_data_required = True
handler.passive_ports = range(port + 1, port + 40)
FTPServer(("127.0.0.1", port), handler).serve_forever()
