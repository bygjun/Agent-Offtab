import Darwin
import Foundation
import Security

private let flags = SecCSFlags(rawValue: 0)
private let maxBytes = 1_048_576

private struct Request: Decodable {
    let account: String
    let data: Data?
}

private struct Response: Encodable {
    let status: OSStatus
    let data: Data?
}

private func isOfftabParent(_ pid: pid_t) -> Bool {
    let expression = "identifier \"local.sessiondesk.app\" and certificate leaf = H\"\(offtabSigner)\""
    let attributes = [kSecGuestAttributePid as String: NSNumber(value: pid)] as CFDictionary
    var requirement: SecRequirement?
    var caller: SecCode?
    guard pid > 1,
          SecRequirementCreateWithString(expression as CFString, flags, &requirement) == errSecSuccess,
          let requirement,
          SecCodeCopyGuestWithAttributes(nil, attributes, flags, &caller) == errSecSuccess,
          let caller,
          SecCodeCheckValidity(caller, flags, requirement) == errSecSuccess else { return false }
    return getppid() == pid
}

private func keychain(_ request: Request) -> Response {
    let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: "Offtab mobile pairing",
        kSecAttrAccount as String: request.account,
    ]
    if let data = request.data {
        let attributes = [kSecValueData as String: data]
        var status = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd(query.merging(attributes) { _, data in data } as CFDictionary, nil)
        }
        return Response(status: status, data: nil)
    }
    var attributes = query
    attributes[kSecReturnData as String] = true
    attributes[kSecMatchLimit as String] = kSecMatchLimitOne
    var value: CFTypeRef?
    let status = SecItemCopyMatching(attributes as CFDictionary, &value)
    return Response(status: status, data: value as? Data)
}

@main
struct PairingKeychain {
    static func main() throws {
        let parent = getppid()
        guard isOfftabParent(parent) else { exit(EX_NOPERM) }
        let input = FileHandle.standardInput
        var bytes = Data()
        while let chunk = try input.read(upToCount: min(65_536, maxBytes + 1 - bytes.count)), !chunk.isEmpty {
            bytes.append(chunk)
            guard bytes.count <= maxBytes else { exit(EX_DATAERR) }
        }
        let request = try JSONDecoder().decode(Request.self, from: bytes)
        guard request.account.utf8.count <= 2_048, !request.account.isEmpty, getppid() == parent else {
            exit(EX_DATAERR)
        }
        let result = keychain(request)
        guard getppid() == parent else { exit(EX_NOPERM) }
        try FileHandle.standardOutput.write(contentsOf: JSONEncoder().encode(result))
    }
}
