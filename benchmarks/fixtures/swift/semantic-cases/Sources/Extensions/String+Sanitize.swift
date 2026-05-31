import Foundation

extension String {
    public func sanitized() -> String {
        return self.trimmingCharacters(in: .whitespaces)
    }
}
