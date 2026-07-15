"""AWS signin password JWE primitives.

The module deliberately turns all cryptographic failures into a constant,
password-free exception. Callers must never log the compact JWE itself.
"""

from __future__ import annotations

import base64
import json
import os
import time
import uuid
from typing import Mapping

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


class JweEncryptionError(RuntimeError):
    """Raised when an AWS password JWE cannot be produced safely."""


def b64url(data: bytes) -> str:
    """Return RFC 7515 base64url without padding."""

    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _decode_b64url(value: str) -> bytes:
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def _public_key_from_jwk(public_key: Mapping[str, str]):
    if public_key.get("kty") not in (None, "RSA"):
        raise ValueError("unsupported key type")
    modulus = int.from_bytes(_decode_b64url(public_key["n"]), "big")
    exponent = int.from_bytes(_decode_b64url(public_key["e"]), "big")
    if modulus <= 0 or exponent <= 1:
        raise ValueError("invalid RSA public key")
    return rsa.RSAPublicNumbers(exponent, modulus).public_key()


def encrypt_password(
    password: str,
    public_key: Mapping[str, str],
    issuer: str,
    audience: str,
    region: str,
    *,
    now: int | None = None,
    cek: bytes | None = None,
    iv: bytes | None = None,
    jti: str | None = None,
) -> str:
    """Encrypt a password as the compact JWE accepted by AWS signin.

    ``cek``, ``iv``, ``now`` and ``jti`` exist for deterministic protocol
    tests. Production callers should leave them unset.
    """

    try:
        issued_at = int(time.time()) if now is None else int(now)
        content_key = AESGCM.generate_key(bit_length=256) if cek is None else cek
        nonce = os.urandom(12) if iv is None else iv
        token_id = str(uuid.uuid4()) if jti is None else jti
        if len(content_key) != 32 or len(nonce) != 12:
            raise ValueError("invalid key material")

        header = {
            "alg": "RSA-OAEP-256",
            "kid": public_key["kid"],
            "enc": "A256GCM",
            "cty": "enc",
            "typ": "application/aws+signin+jwe",
        }
        encoded_header = b64url(
            json.dumps(header, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        )
        encrypted_key = _public_key_from_jwk(public_key).encrypt(
            content_key,
            padding.OAEP(
                mgf=padding.MGF1(algorithm=hashes.SHA256()),
                algorithm=hashes.SHA256(),
                label=None,
            ),
        )
        claims = {
            "iss": f"{region}.{issuer}",
            "iat": issued_at,
            "nbf": issued_at,
            "jti": token_id,
            "exp": issued_at + 300,
            "aud": f"{region}.{audience}",
            "password": password,
        }
        plaintext = json.dumps(
            claims, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")
        encrypted = AESGCM(content_key).encrypt(
            nonce, plaintext, encoded_header.encode("ascii")
        )
        ciphertext, tag = encrypted[:-16], encrypted[-16:]
        return ".".join(
            (
                encoded_header,
                b64url(encrypted_key),
                b64url(nonce),
                b64url(ciphertext),
                b64url(tag),
            )
        )
    except Exception as exc:
        raise JweEncryptionError("password encryption failed") from exc
