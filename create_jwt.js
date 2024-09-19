import { create, getNumericDate } from "https://deno.land/x/djwt@v3.0.2/mod.ts";

export async function main({
    secret_key,
    payload,
}) {
    let encoder = new TextEncoder()
    var keyBuf = encoder.encode(secret_key);

    var key = await crypto.subtle.importKey(
        "raw",
        keyBuf,
        { name: "HMAC", hash: "SHA-256" },
        true,
        ["sign", "verify"],
    )

    const algorithm = "HS256"

    const header = {
        alg: algorithm,
        typ: "JWT",
    };

    let payloadData = JSON.parse(payload)

    payloadData.exp = getNumericDate(1000);

    const token = await create(header, payloadData, key);
    console.log(token)

    return token
}
