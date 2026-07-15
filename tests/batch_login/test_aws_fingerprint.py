import json
import random
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.aws_fingerprint import (
    AppJsConfig,
    FALLBACK_CONFIG,
    build_fingerprint_data,
    crc32,
    encrypt_fingerprint,
    extract_app_js_config,
    new_fingerprint_context,
    random_identity,
    xxtea_encrypt,
)


class AwsFingerprintPrimitiveTests(unittest.TestCase):
    def test_crc32_matches_ieee_vector(self):
        self.assertEqual(0xCBF43926, crc32("123456789"))

    def test_xxtea_matches_reference_javascript_vector(self):
        encrypted = xxtea_encrypt("hello", FALLBACK_CONFIG.key)
        self.assertEqual("b7e02d5854503aa5", encrypted.hex())

    def test_extracts_dynamic_config_from_minified_app_js(self):
        js = (
            'var q=[2576816180,"DynamicId9",874813317,1888420705,2347232058];'
            'globalThis.FWCIM_VERSION="4.9.2";'
        )
        config = extract_app_js_config(js)

        self.assertEqual(
            AppJsConfig(
                key=(1888420705, 2576816180, 2347232058, 874813317),
                identifier="DynamicId9",
                version="4.9.2",
            ),
            config,
        )

    def test_incomplete_dynamic_config_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "AWS app.js fingerprint config"):
            extract_app_js_config('var q=[1,"OnlyKey",2,3,4];')

    def test_encrypted_fingerprint_has_identifier_prefix_and_reference_payload(self):
        encrypted = encrypt_fingerprint('{"a":1}', FALLBACK_CONFIG)
        self.assertEqual("ECdITeCs:ywV57pWlNmt5SOqlFWw6Ag==", encrypted)


class AwsBrowserIdentityTests(unittest.TestCase):
    def test_random_identity_is_internally_consistent(self):
        identity = random_identity(random.Random(7))

        self.assertIn(f"Chrome/{identity.chrome_version}", identity.user_agent)
        self.assertEqual(256, len(identity.histogram_base))
        self.assertEqual(36_000, sum(identity.histogram_base))
        self.assertEqual(sorted(identity.webgl_extensions), identity.webgl_extensions)
        self.assertGreaterEqual(identity.screen.avail_width, identity.screen.width)
        self.assertGreaterEqual(identity.screen.height, identity.screen.avail_height)

    def test_fingerprint_json_keeps_aws_field_order_and_context_identity(self):
        rng = random.Random(11)
        identity = random_identity(rng)
        context = new_fingerprint_context(identity, now_seconds=1_700_000_000, rng=rng)

        data = build_fingerprint_data(
            identity=identity,
            location_url="https://us-east-1.signin.aws/platform/d-123/login",
            referrer="https://portal.sso.us-east-1.amazonaws.com/",
            now_ms=1_700_000_010_000,
            context=context,
            page_type="profile",
            event_type="PageLoad",
            time_on_page=0,
            email_length=0,
            email="",
            config=AppJsConfig((1, 2, 3, 4), "Test", "9.8.7"),
            rng=rng,
        )
        payload = data.to_json()
        decoded = json.loads(payload)

        self.assertEqual(
            [
                "metrics", "start", "interaction", "scripts", "history",
                "battery", "performance", "automation", "end", "timeZone",
            ],
            list(decoded)[:10],
        )
        self.assertEqual(identity.user_agent, decoded["userAgent"])
        self.assertEqual(identity.canvas_hash, decoded["canvas"]["hash"])
        self.assertEqual(context.ls_ubid_profile, decoded["lsUbid"])
        self.assertEqual("9.8.7", decoded["version"])
        self.assertFalse(decoded["webDriver"])


if __name__ == "__main__":
    unittest.main()
