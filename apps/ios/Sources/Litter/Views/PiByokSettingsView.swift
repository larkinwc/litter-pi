import SwiftUI

/// Pi runtime BYOK entry path.
///
/// Lets the user enter either an Anthropic API key or an OpenAI-compatible
/// base URL + key and start an in-process pi session through
/// `AppClient.connectLocalPi(...)`. The full Anthropic OAuth flow is
/// stubbed (see `AnthropicOAuthSheet`); it lands in the auth milestone.
struct PiByokSettingsView: View {
    enum Provider: String, CaseIterable, Identifiable {
        case anthropic
        case openaiCompatible

        var id: String { rawValue }
        var label: String {
            switch self {
            case .anthropic: return "Anthropic (BYOK)"
            case .openaiCompatible: return "OpenAI-compatible (BYOK)"
            }
        }
        var providerId: String {
            switch self {
            case .anthropic: return "anthropic"
            case .openaiCompatible: return "openai"
            }
        }
    }

    @AppStorage("piByokProvider") private var providerRaw: String = Provider.anthropic.rawValue
    @AppStorage("piByokBaseURL") private var baseURL: String = ""
    @AppStorage("piByokModel") private var model: String = ""
    @State private var apiKey: String = ""
    @State private var status: String?
    @State private var isStarting: Bool = false
    @State private var startedServerId: String?
    @State private var showOAuthSheet: Bool = false

    private let client = AppClient()

    private var provider: Provider {
        Provider(rawValue: providerRaw) ?? .anthropic
    }

    var body: some View {
        Form {
            Section {
                Picker("Provider", selection: $providerRaw) {
                    ForEach(Provider.allCases) { p in
                        Text(p.label).tag(p.rawValue)
                    }
                }
                .pickerStyle(.menu)
                .listRowBackground(LitterTheme.surface.opacity(0.6))

                SecureField("API key", text: $apiKey)
                    .autocorrectionDisabled(true)
                    .textInputAutocapitalization(.never)
                    .listRowBackground(LitterTheme.surface.opacity(0.6))

                if provider == .openaiCompatible {
                    TextField("https://api.example.com/v1", text: $baseURL)
                        .autocorrectionDisabled(true)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.URL)
                        .listRowBackground(LitterTheme.surface.opacity(0.6))
                }

                TextField("Model (optional)", text: $model)
                    .autocorrectionDisabled(true)
                    .textInputAutocapitalization(.never)
                    .listRowBackground(LitterTheme.surface.opacity(0.6))
            } header: {
                Text("BYOK")
                    .foregroundColor(LitterTheme.textSecondary)
            } footer: {
                Text(provider == .openaiCompatible
                    ? "The supplied base URL is the one used at request time."
                    : "Used to drive an in-process pi session against Anthropic.")
                    .litterFont(.caption)
                    .foregroundColor(LitterTheme.textMuted)
            }

            Section {
                Button {
                    Task { await startPiSession() }
                } label: {
                    HStack {
                        Image(systemName: "play.circle.fill")
                            .foregroundColor(LitterTheme.accent)
                        Text(isStarting ? "Starting…" : "Start pi session")
                            .litterFont(.subheadline)
                            .foregroundColor(LitterTheme.accent)
                        Spacer()
                    }
                }
                .disabled(isStarting || apiKey.isEmpty || (provider == .openaiCompatible && baseURL.isEmpty))
                .listRowBackground(LitterTheme.surface.opacity(0.6))

                Button {
                    showOAuthSheet = true
                } label: {
                    HStack {
                        Image(systemName: "key.horizontal")
                            .foregroundColor(LitterTheme.textSecondary)
                        Text("Sign in with Anthropic (preview)")
                            .litterFont(.subheadline)
                            .foregroundColor(LitterTheme.textSecondary)
                        Spacer()
                    }
                }
                .listRowBackground(LitterTheme.surface.opacity(0.6))

                if let startedServerId {
                    Text("Connected as \(startedServerId)")
                        .litterFont(.caption)
                        .foregroundColor(LitterTheme.textSecondary)
                        .listRowBackground(LitterTheme.surface.opacity(0.6))
                }
                if let status {
                    Text(status)
                        .litterFont(.caption)
                        .foregroundColor(LitterTheme.textMuted)
                        .listRowBackground(LitterTheme.surface.opacity(0.6))
                }
            } header: {
                Text("Session")
                    .foregroundColor(LitterTheme.textSecondary)
            }
        }
        .scrollContentBackground(.hidden)
        .background(LitterTheme.backgroundGradient.ignoresSafeArea())
        .navigationTitle("Pi runtime")
        .navigationBarTitleDisplayMode(.inline)
        .sheet(isPresented: $showOAuthSheet) {
            AnthropicOAuthSheet()
        }
    }

    private func startPiSession() async {
        isStarting = true
        defer { isStarting = false }
        status = nil
        let serverId = "pi-local-\(UUID().uuidString.prefix(8))"
        let displayName = provider == .anthropic
            ? "Local pi (Anthropic BYOK)"
            : "Local pi (OpenAI-compatible BYOK)"
        do {
            let returnedId = try await client.connectLocalPiByok(
                serverId: serverId,
                displayName: displayName,
                provider: provider.providerId,
                apiKey: apiKey,
                baseUrl: provider == .openaiCompatible
                    ? baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
                    : nil,
                model: model.isEmpty ? nil : model
            )
            startedServerId = returnedId
            let baseURLNote = provider == .openaiCompatible
                ? " base=\(baseURL)"
                : ""
            status = "Pi session started.\(baseURLNote)"
        } catch {
            status = "Failed to start pi session: \(error.localizedDescription)"
        }
    }
}

#Preview {
    NavigationStack {
        PiByokSettingsView()
    }
}
