{
  description = "Fixture flake for nh system tests";

  outputs = { self }: {
    systemConfigs.default = {
      config = {
        system = {
          name = "default";
        };
      };
    };
  };
}
