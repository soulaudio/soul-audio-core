// SoulProfile — a CoreAudio HAL plug-in (AudioServerPlugIn) that impersonates device
// classes we don't own, so CI can run the real AUHAL exclusive path against them.
//
// CI TEST FIXTURE ONLY: built and installed on hosted macOS runners by
// .github/workflows/ci-macos-audio.yml; never linked into or shipped with any crate.
// Built on libASPL (MIT, https://github.com/gavv/libASPL).
//
// Each device advertises a discrete nominal-rate ladder and *integer* physical formats
// (BlackHole is Float32-only, so without this the HAL's float -> integer conversion on
// the exclusive path is never exercised). Every device also has an input stream that
// loops back what was written to the output, sample-time indexed like BlackHole, so the
// bit-perfect loopback test can run against integer physical formats too:
//
//   output physical (Int16 / Int24 packed / Int32) --> ring of left-justified Int32
//   --> input physical (always Int32) --> HAL --> Float32 capture client
//
// Int32 holds any of the output words losslessly, and a left-justified <=24-bit word
// converts to Float32 exactly, so a capture is bit-identical iff the output path was.

#include <aspl/Driver.hpp>

#include <CoreAudio/AudioServerPlugIn.h>

#include <atomic>
#include <cstring>
#include <memory>
#include <vector>

namespace {

struct Profile {
    const char* name;
    const char* uid;
    std::vector<Float64> rates; // discrete nominal rates
    std::vector<UInt32> bits;   // packed signed-integer output words: 16, 24 (3-byte), 32
    Float64 initialRate;
    UInt32 initialBits;
};

// Keep in sync with the `hal-profile-device` CI job, which asserts each device's
// captured profile against these tables.
const std::vector<Profile>& Profiles()
{
    static const std::vector<Profile> profiles = {
        // A typical USB Audio Class 2 DAC (cf. fixtures/coreaudio/usb-uac2-dac-384k.json):
        // discrete 44.1k..384k, Int16 / Int24 / Int32 at every rate.
        {"SoulProfile UAC2 DAC",
            "audio.soulaudio.profile.uac2",
            {44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000},
            {16, 24, 32},
            44100,
            16},
        // An HDMI receiver on Apple silicon (cf. hdmi-avr-apple-silicon.json):
        // 48 kHz only, Int16 / Int24 — the 44.1k family has nowhere native to go.
        {"SoulProfile HDMI 48k",
            "audio.soulaudio.profile.hdmi48",
            {48000},
            {16, 24},
            48000,
            16},
    };
    return profiles;
}

constexpr UInt32 kChannels = 2;
constexpr UInt32 kRingFrames = 1u << 17; // > 0.34 s at 384 kHz

AudioStreamBasicDescription IntFormat(Float64 rate, UInt32 bits)
{
    const UInt32 bytes = bits / 8;
    AudioStreamBasicDescription f = {};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags =
        kAudioFormatFlagIsSignedInteger | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBitsPerChannel = bits;
    f.mChannelsPerFrame = kChannels;
    f.mBytesPerFrame = bytes * kChannels;
    f.mFramesPerPacket = 1;
    f.mBytesPerPacket = bytes * kChannels;
    return f;
}

AudioStreamBasicDescription FloatFormat(Float64 rate)
{
    AudioStreamBasicDescription f = {};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags =
        kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBitsPerChannel = 32;
    f.mChannelsPerFrame = kChannels;
    f.mBytesPerFrame = 4 * kChannels;
    f.mFramesPerPacket = 1;
    f.mBytesPerPacket = 4 * kChannels;
    return f;
}

// State shared by a device, its two streams and its I/O handler.
struct State {
    explicit State(const Profile& p)
        : profile(p)
        , rate(p.initialRate)
        , ring(kRingFrames * kChannels, 0)
    {
    }
    const Profile& profile;
    std::atomic<Float64> rate;
    std::vector<SInt32> ring; // left-justified samples, indexed by sample time
};

class ProfileStream : public aspl::Stream
{
public:
    ProfileStream(std::shared_ptr<const aspl::Context> context,
        std::shared_ptr<aspl::Device> device,
        const aspl::StreamParameters& params,
        std::shared_ptr<State> state,
        std::vector<UInt32> bits,
        UInt32 initialBits)
        : Stream(std::move(context), std::move(device), params)
        , state_(std::move(state))
        , bits_(std::move(bits))
        , current_(initialBits)
    {
    }

    // The nominal rate is device-wide: formats always report the device's current rate.
    AudioStreamBasicDescription GetPhysicalFormat() const override
    {
        return IntFormat(state_->rate.load(), current_.load());
    }

    std::vector<AudioStreamRangedDescription> GetAvailablePhysicalFormats() const override
    {
        std::vector<AudioStreamRangedDescription> out;
        for (Float64 r : state_->profile.rates) {
            for (UInt32 b : bits_) {
                AudioStreamRangedDescription d = {};
                d.mFormat = IntFormat(r, b);
                d.mSampleRateRange.mMinimum = r;
                d.mSampleRateRange.mMaximum = r;
                out.push_back(d);
            }
        }
        return out;
    }

    AudioStreamBasicDescription GetVirtualFormat() const override
    {
        return FloatFormat(state_->rate.load());
    }

    std::vector<AudioStreamRangedDescription> GetAvailableVirtualFormats() const override
    {
        std::vector<AudioStreamRangedDescription> out;
        for (Float64 r : state_->profile.rates) {
            AudioStreamRangedDescription d = {};
            d.mFormat = FloatFormat(r);
            d.mSampleRateRange.mMinimum = r;
            d.mSampleRateRange.mMaximum = r;
            out.push_back(d);
        }
        return out;
    }

protected:
    OSStatus SetPhysicalFormatImpl(const AudioStreamBasicDescription& f) override
    {
        current_ = f.mBitsPerChannel;
        state_->rate = f.mSampleRate; // a physical format carries a rate, like real HW
        return kAudioHardwareNoError;
    }

    OSStatus SetVirtualFormatImpl(const AudioStreamBasicDescription& f) override
    {
        state_->rate = f.mSampleRate;
        return kAudioHardwareNoError;
    }

private:
    std::shared_ptr<State> state_;
    std::vector<UInt32> bits_;
    std::atomic<UInt32> current_;
};

class ProfileDevice : public aspl::Device
{
public:
    ProfileDevice(std::shared_ptr<const aspl::Context> context,
        const aspl::DeviceParameters& params,
        std::shared_ptr<State> state)
        : Device(std::move(context), params)
        , state_(std::move(state))
    {
    }

    Float64 GetNominalSampleRate() const override
    {
        return state_->rate.load();
    }

    std::vector<AudioValueRange> GetAvailableSampleRates() const override
    {
        std::vector<AudioValueRange> out;
        for (Float64 r : state_->profile.rates) {
            out.push_back({r, r});
        }
        return out;
    }

protected:
    OSStatus SetNominalSampleRateImpl(Float64 rate) override
    {
        state_->rate = rate;
        return kAudioHardwareNoError;
    }

private:
    std::shared_ptr<State> state_;
};

class LoopbackHandler : public aspl::IORequestHandler
{
public:
    explicit LoopbackHandler(std::shared_ptr<State> state)
        : state_(std::move(state))
    {
    }

    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>& stream,
        Float64 /*zeroTimestamp*/,
        Float64 timestamp,
        const void* bytes,
        UInt32 bytesCount) override
    {
        const auto fmt = stream->GetPhysicalFormat();
        const UInt32 width = fmt.mBitsPerChannel / 8;
        const UInt32 samples = bytesCount / width;
        const auto* in = static_cast<const UInt8*>(bytes);
        UInt64 slot = (UInt64(timestamp) % kRingFrames) * kChannels;
        for (UInt32 i = 0; i < samples; i++, in += width) {
            SInt32 v = 0;
            switch (width) {
            case 2: {
                SInt16 s;
                std::memcpy(&s, in, 2);
                v = SInt32(s) << 16;
                break;
            }
            case 3: // packed little-endian 24-bit
                v = SInt32(UInt32(in[0]) << 8 | UInt32(in[1]) << 16 | UInt32(in[2]) << 24);
                break;
            case 4:
                std::memcpy(&v, in, 4);
                break;
            }
            state_->ring[slot] = v;
            slot = (slot + 1) % (kRingFrames * kChannels);
        }
    }

    void OnReadClientInput(const std::shared_ptr<aspl::Client>& /*client*/,
        const std::shared_ptr<aspl::Stream>& /*stream*/,
        Float64 /*zeroTimestamp*/,
        Float64 timestamp,
        void* bytes,
        UInt32 bytesCount) override
    {
        // The input stream is always Int32: one ring sample per output sample.
        auto* out = static_cast<SInt32*>(bytes);
        const UInt32 samples = bytesCount / 4;
        UInt64 slot = (UInt64(timestamp) % kRingFrames) * kChannels;
        for (UInt32 i = 0; i < samples; i++) {
            out[i] = state_->ring[slot];
            slot = (slot + 1) % (kRingFrames * kChannels);
        }
    }

private:
    std::shared_ptr<State> state_;
};

std::shared_ptr<aspl::Driver> CreateDriver()
{
    auto context = std::make_shared<aspl::Context>();
    auto plugin = std::make_shared<aspl::Plugin>(context);

    for (const auto& p : Profiles()) {
        auto state = std::make_shared<State>(p);

        aspl::DeviceParameters dp;
        dp.Name = p.name;
        dp.Manufacturer = "Soul Audio CI";
        dp.DeviceUID = p.uid;
        dp.ModelUID = "audio.soulaudio.profile";
        dp.SampleRate = UInt32(p.initialRate);
        dp.ChannelCount = kChannels;
        dp.EnableMixing = true;
        // Don't let it become the system default output; tests address it by name.
        dp.CanBeDefault = false;
        dp.CanBeDefaultForSystemSounds = false;

        auto device = std::make_shared<ProfileDevice>(context, dp, state);

        aspl::StreamParameters out;
        out.Direction = aspl::Direction::Output;
        out.Format = IntFormat(p.initialRate, p.initialBits);
        device->AddStreamAsync(std::make_shared<ProfileStream>(
            context, device, out, state, p.bits, p.initialBits));

        aspl::StreamParameters in;
        in.Direction = aspl::Direction::Input;
        in.Format = IntFormat(p.initialRate, 32);
        device->AddStreamAsync(std::make_shared<ProfileStream>(
            context, device, in, state, std::vector<UInt32>{32}, 32));

        device->SetIOHandler(std::make_shared<LoopbackHandler>(state));
        plugin->AddDevice(device);
    }

    return std::make_shared<aspl::Driver>(context, plugin);
}

} // namespace

extern "C" __attribute__((visibility("default"))) void* SoulProfileEntryPoint(CFAllocatorRef /*allocator*/, CFUUIDRef typeUUID)
{
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }
    static std::shared_ptr<aspl::Driver> driver = CreateDriver();
    return driver->GetReference();
}
