import Foundation

/// Entity visual state — mirrors the `EntityState` enum in dexter.proto.
///
/// Proto types do not leak past DexterClient. EntityState is the Swift-layer
/// representation used by AnimatedEntity and EntityRenderer; callers never
/// import generated proto modules to inspect or transition state.
enum EntityState {
    case idle, listening, thinking, speaking, alert, focused

    /// Map an incoming proto enum value to the Swift representation.
    /// Both `.unspecified` and `.idle` collapse to `.idle` — unspecified is
    /// a protobuf sentinel that means "not set", semantically identical to idle
    /// from the Swift layer's perspective.
    init(from proto: Dexter_V1_EntityState) {
        switch proto {
        case .listening:   self = .listening
        case .thinking:    self = .thinking
        case .speaking:    self = .speaking
        case .alert:       self = .alert
        case .focused:     self = .focused
        default:           self = .idle
        }
    }

    /// Integer value passed to the Metal shader's `Uniforms.state` field.
    /// Must stay in sync with the switch table in the fragment shader source
    /// in GeometricRenderer.swift — both are updated together if states change.
    var shaderValue: Int32 {
        switch self {
        case .idle:      return 0
        case .listening: return 1
        case .thinking:  return 2
        case .speaking:  return 3
        case .alert:     return 4
        case .focused:   return 5
        }
    }
}
