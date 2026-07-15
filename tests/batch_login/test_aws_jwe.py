import base64
import json
import sys
import unittest
from pathlib import Path

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.aws_jwe import JweEncryptionError, b64url, encrypt_password


def decode_b64url(value: str) -> bytes:
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


class AwsJweTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        public_numbers = cls.private_key.public_key().public_numbers()
        cls.public_jwk = {
            "kty": "RSA",
            "kid": "synthetic-key",
            "n": b64url(public_numbers.n.to_bytes(256, "big")),
            "e": b64url(public_numbers.e.to_bytes(3, "big")),
        }

    def test_base64url_is_unpadded(self):
        self.assertEqual("--__", b64url(b"\xfb\xef\xff"))
        self.assertNotIn("=", b64url(b"one byte"))

    def test_encrypt_password_builds_aws_compact_jwe(self):
        token = encrypt_password(
            "synthetic-password",
            self.public_jwk,
            issuer="signin",
            audience="portal",
            region="us-east-1",
            now=1_700_000_000,
            cek=b"C" * 32,
            iv=b"I" * 12,
            jti="00000000-0000-0000-0000-000000000001",
        )

        parts = token.split(".")
        self.assertEqual(5, len(parts))
        self.assertTrue(all("=" not in part for part in parts))
        header = json.loads(decode_b64url(parts[0]))
        self.assertEqual(
            {
                "alg": "RSA-OAEP-256",
                "kid": "synthetic-key",
                "enc": "A256GCM",
                "cty": "enc",
                "typ": "application/aws+signin+jwe",
            },
            header,
        )

        cek = self.private_key.decrypt(
            decode_b64url(parts[1]),
            padding.OAEP(
                mgf=padding.MGF1(algorithm=hashes.SHA256()),
                algorithm=hashes.SHA256(),
                label=None,
            ),
        )
        claims = json.loads(
            AESGCM(cek).decrypt(
                decode_b64url(parts[2]),
                decode_b64url(parts[3]) + decode_b64url(parts[4]),
                parts[0].encode("ascii"),
            )
        )
        self.assertEqual("us-east-1.signin", claims["iss"])
        self.assertEqual("us-east-1.portal", claims["aud"])
        self.assertEqual(1_700_000_000, claims["iat"])
        self.assertEqual(1_700_000_000, claims["nbf"])
        self.assertEqual(1_700_000_300, claims["exp"])
        self.assertEqual("00000000-0000-0000-0000-000000000001", claims["jti"])
        self.assertEqual("synthetic-password", claims["password"])

    def test_invalid_key_error_does_not_expose_password(self):
        secret = "do-not-leak-this-secret"
        with self.assertRaises(JweEncryptionError) as raised:
            encrypt_password(
                secret,
                {"kty": "RSA", "kid": "broken", "n": "!", "e": "AQAB"},
                issuer="signin",
                audience="portal",
                region="us-east-1",
            )

        self.assertNotIn(secret, str(raised.exception))
        self.assertNotIn(secret, repr(raised.exception))

    def test_rejects_invalid_injected_key_material_without_leaking_password(self):
        secret = "another-synthetic-secret"
        with self.assertRaises(JweEncryptionError) as raised:
            encrypt_password(
                secret,
                self.public_jwk,
                issuer="signin",
                audience="portal",
                region="us-east-1",
                cek=b"short",
                iv=b"I" * 12,
            )

        self.assertEqual("password encryption failed", str(raised.exception))
        self.assertNotIn(secret, repr(raised.exception))


if __name__ == "__main__":
    unittest.main()
